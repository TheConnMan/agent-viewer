package com.theconnman.agentviewer

import android.app.Activity
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.EditorInfo
import com.termux.terminal.TextStyle
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject
import okhttp3.Request
import okhttp3.WebSocket
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.Robolectric
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The client half of the terminal wire protocol, driven through the real Termux view, the real
 * [WebSocketTerminalTransport], and the real socket listener.
 *
 * Four tests are transport-proving and fail against a stub, because the proof is exact bytes and
 * JSON arriving at the fake socket rather than calls being made: the layout resize frame, the
 * key and IME byte tests, the over-limit input chunking, and the close control frame. The rest
 * never reach the wire and would pass with any transport - the binary render, the pre-layout
 * replay, the exit frame, control-frame parsing, the close-code rules, the backoff curve, and
 * the capability decoding. They cover the other halves of the same client surface: what the
 * emulator does with server output, and what the client decides when a socket goes away.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w411dp-h891dp-xxhdpi")
// NATIVE graphics: the legacy Paint stub measures every glyph as 0 wide and the view
// divides by the font width to get its column count.
@GraphicsMode(GraphicsMode.Mode.NATIVE)
class TerminalProtocolTest {

    private companion object {
        const val ESC = "\u001b"
        const val PHONE_W = 1080
        const val PHONE_H = 1800
    }

    /** Records every frame the client puts on the wire, keeping binary and text apart. */
    private class FakeWebSocket : WebSocket {
        val binary = mutableListOf<ByteString>()
        val text = mutableListOf<String>()

        override fun request(): Request = Request.Builder().url("http://127.0.0.1/").build()
        override fun queueSize(): Long = 0
        override fun send(text: String): Boolean = this.text.add(text)
        override fun send(bytes: ByteString): Boolean = binary.add(bytes)
        override fun close(code: Int, reason: String?): Boolean = true
        override fun cancel() = Unit

        fun drainBinary(): String {
            val joined = binary.joinToString("") { it.utf8() }
            binary.clear()
            return joined
        }
    }

    private class Rig(laidOut: Boolean = true) {
        val socket = FakeWebSocket()
        val transport = WebSocketTerminalTransport { socket }
        var exited = false
        val controller: RemoteTerminalController

        init {
            val activity = Robolectric.buildActivity(Activity::class.java).setup().get()
            controller = RemoteTerminalController(activity, transport) { exited = true }
            activity.setContentView(controller.view)
            if (laidOut) layOut()
        }

        /** The measure and layout pass that gives the view a size and so builds the emulator. */
        fun layOut() {
            controller.view.measure(
                View.MeasureSpec.makeMeasureSpec(PHONE_W, View.MeasureSpec.EXACTLY),
                View.MeasureSpec.makeMeasureSpec(PHONE_H, View.MeasureSpec.EXACTLY),
            )
            controller.view.layout(0, 0, PHONE_W, PHONE_H)
        }

        /** The real listener, with the main-thread hop collapsed so a test can drive it inline. */
        fun listener(connection: TerminalConnection, onLeave: () -> Unit = {}) =
            terminalSocketListener(
                connection = connection,
                controller = controller,
                onSocket = {},
                onLeave = onLeave,
                dispatch = { it() },
            )

        fun key(keyCode: Int, meta: Int = 0) {
            controller.view.onKeyDown(
                keyCode,
                KeyEvent(0, 0, KeyEvent.ACTION_DOWN, keyCode, 0, meta),
            )
        }

        fun screenText(): String = controller.view.mEmulator.screen.transcriptText
    }

    @Test
    fun `layout announces geometry as a resize control frame carrying cols and rows`() {
        val rig = Rig()

        val resize = rig.socket.text
            .map { ApiJson.json.parseToJsonElement(it).jsonObject }
            .last { it["type"] == JsonPrimitive("resize") }
        val emulator = rig.controller.view.mEmulator

        assertEquals(JsonPrimitive("resize"), resize["type"])
        assertEquals(setOf("type", "cols", "rows"), resize.keys)
        assertEquals(JsonPrimitive(emulator.mColumns), resize["cols"])
        assertEquals(JsonPrimitive(emulator.mRows), resize["rows"])
        assertTrue("a laid out view has real columns", emulator.mColumns > 20)
        assertTrue("no PTY bytes are sent by layout alone", rig.socket.binary.isEmpty())
    }

    @Test
    fun `a binary frame of PTY output is rendered by the emulator with its colors`() {
        val rig = Rig()

        val ptyOutput = "${ESC}[31mred${ESC}[0m ok\r\n".toByteArray(Charsets.UTF_8)
        rig.controller.receive(ptyOutput)

        val text = rig.screenText()
        assertTrue("PTY output on screen:\n$text", text.contains("red ok"))
        assertTrue("escape sequences are consumed, not printed", !text.contains(ESC))
        assertTrue("the red SGR colored a cell", anyColoredCell(rig))
    }

    @Test
    fun `output that arrives before the view is laid out is replayed onto the first screen`() {
        val rig = Rig(laidOut = false)
        assertNull("an unmeasured view has no emulator yet", rig.controller.view.mEmulator)

        // The console opens its socket as soon as it composes, so the hello and the server's
        // one-shot redraw both land in this window. Dropping them left the screen blank until
        // the CLI happened to repaint on its own.
        rig.controller.receive("${ESC}[31mredraw${ESC}[0m before layout\r\n".toByteArray())
        rig.controller.receive("second chunk\r\n".toByteArray())
        rig.layOut()

        val text = rig.screenText()
        assertTrue("the replayed redraw is on screen:\n$text", text.contains("redraw before layout"))
        assertTrue("replay keeps arrival order:\n$text", text.indexOf("redraw") < text.indexOf("second chunk"))
        assertTrue("escape sequences are consumed, not printed", !text.contains(ESC))
        assertTrue("the replayed SGR colored a cell", anyColoredCell(rig))

        // Later bytes go straight through, and the buffer does not replay a second time on the
        // resize-driven calls that follow.
        rig.controller.receive("live after layout\r\n".toByteArray())
        rig.layOut()
        val after = rig.screenText()
        assertEquals("the replay ran once", 1, after.split("redraw before layout").size - 1)
        assertTrue("live output still lands:\n$after", after.contains("live after layout"))
    }

    @Test
    fun `a takeover close is final and stops the client reconnecting`() {
        val rig = Rig()
        val connection = TerminalConnection()
        var left = false
        val listener = rig.listener(connection) { left = true }

        listener.onClosed(rig.socket, TERMINAL_CLOSE_REPLACED, "replaced")

        assertEquals(4_000, TERMINAL_CLOSE_REPLACED)
        assertTrue("the takeover is recorded", connection.replaced)
        assertTrue("and it is terminal for this console", connection.finished)
        assertEquals("replaced by another connection", connection.status)
        assertTrue("no reconnect is owed", !connection.reconnectScheduled)
        assertEquals("a takeover is not a failure to back off from", 0, connection.failures)
        assertTrue("the console stays up rather than popping back to the list", !left)
    }

    @Test
    fun `a slow consumer close reconnects like any other drop`() {
        val rig = Rig()
        val connection = TerminalConnection()
        val listener = rig.listener(connection)

        listener.onClosed(rig.socket, 1_013, "slow consumer")

        assertTrue("1013 is a drop, and the redraw restores the screen", connection.reconnectScheduled)
        assertEquals("disconnected", connection.status)
        assertEquals(1, connection.failures)
        assertTrue("the console is still live", !connection.finished)

        // A takeover arriving after a plain drop still cancels the reconnect that drop scheduled.
        listener.onClosed(rig.socket, TERMINAL_CLOSE_REPLACED, "replaced")
        assertTrue(!connection.reconnectScheduled)
        assertTrue(connection.finished)
    }

    @Test
    fun `the close this client asked for hands back to the list`() {
        val rig = Rig()
        val connection = TerminalConnection()
        var left = false
        val listener = rig.listener(connection) { left = true }
        connection.closeRequested = true

        listener.onClosed(rig.socket, 1_000, "closed")

        assertTrue("a requested close pops the console", left)
        assertTrue("and schedules nothing", !connection.reconnectScheduled)
    }

    @Test
    fun `the listener renders binary frames and ends the session on an exit frame`() {
        val rig = Rig()
        val connection = TerminalConnection()
        val listener = rig.listener(connection)

        listener.onMessage(rig.socket, """{"type":"hello","cols":80,"rows":24,"running":true}""")
        listener.onMessage(rig.socket, "from the socket\r\n".toByteArray().toByteString())
        assertEquals("80x24", connection.geometry)
        assertTrue(rig.screenText().contains("from the socket"))

        listener.onMessage(rig.socket, """{"type":"exit","status":0}""")
        assertEquals("session ended", connection.status)
        assertTrue("an exit stops the reconnect loop", connection.finished)
        listener.onClosed(rig.socket, 1_000, "done")
        assertTrue("and the close that follows it schedules nothing", !connection.reconnectScheduled)
    }

    @Test
    fun `keys and IME text leave as binary frames of exactly the PTY bytes`() {
        val rig = Rig()
        rig.socket.binary.clear()

        val ime = rig.controller.view.onCreateInputConnection(EditorInfo())
        assertNotNull(ime)
        ime.commitText("hello", 1)
        assertEquals("hello", rig.socket.drainBinary())

        rig.key(KeyEvent.KEYCODE_ENTER)
        assertEquals("\r", rig.socket.drainBinary())

        rig.key(KeyEvent.KEYCODE_TAB)
        assertEquals("\t", rig.socket.drainBinary())

        rig.key(KeyEvent.KEYCODE_ESCAPE)
        assertEquals(ESC, rig.socket.drainBinary())

        rig.key(KeyEvent.KEYCODE_DPAD_UP)
        val up = rig.socket.drainBinary()
        assertTrue("arrow up as CSI or SS3", up == "$ESC[A" || up == "${ESC}OA")

        // Sticky CTRL from the key row is read back through TerminalViewClient and consumed once.
        rig.controller.stickyControl = true
        ime.commitText("c", 1)
        assertEquals("\u0003", rig.socket.drainBinary())
        assertTrue("sticky ctrl is consumed on read", !rig.controller.stickyControl)
        ime.commitText("c", 1)
        assertEquals("c", rig.socket.drainBinary())

        // Key row ctrl-c goes straight through the session, not the keyboard path.
        rig.controller.write("\u0003")
        assertEquals("\u0003", rig.socket.drainBinary())

        rig.controller.paste("pasted")
        assertEquals("paste is plain until the CLI asks for brackets", "pasted", rig.socket.drainBinary())

        // Both CLIs turn bracketed paste on; the emulator honours it from the PTY stream alone.
        rig.controller.receive("$ESC[?2004h".toByteArray(Charsets.UTF_8))
        rig.controller.paste("pasted")
        assertEquals("$ESC[200~pasted$ESC[201~", rig.socket.drainBinary())

        assertTrue("every frame above was binary", rig.socket.text.none { it.contains("input") })
    }

    @Test
    fun `every way a keyboard can say enter reaches the PTY as a carriage return`() {
        // Claude Code's composer submits on CR and inserts a line on LF. Hardware and most soft
        // keyboards send KEYCODE_ENTER (a CR through KeyHandler); the AOSP keyboard family commits
        // a literal "\n" as text; and dictation keyboards such as Wispr Flow press their own
        // Enter through performEditorAction, which BaseInputConnection would otherwise swallow.
        val rig = Rig()
        rig.socket.binary.clear()
        val ime = rig.controller.view.onCreateInputConnection(EditorInfo())
        assertNotNull(ime)

        ime.commitText("hello\n", 1)
        assertEquals("a committed newline is enter, not a line feed", "hello\r", rig.socket.drainBinary())

        assertTrue(ime.performEditorAction(EditorInfo.IME_ACTION_UNSPECIFIED))
        assertEquals("\r", rig.socket.drainBinary())
        assertTrue(ime.performEditorAction(EditorInfo.IME_ACTION_DONE))
        assertEquals("\r", rig.socket.drainBinary())
        assertTrue(ime.performEditorAction(EditorInfo.IME_ACTION_SEND))
        assertEquals("\r", rig.socket.drainBinary())

        rig.key(KeyEvent.KEYCODE_ENTER)
        assertEquals("\r", rig.socket.drainBinary())

        // Multi-line paste is the emulator's job and keeps its own rules: LF becomes CR there too,
        // wrapped in brackets once the CLI has asked for them, so a pasted paragraph is one paste.
        rig.controller.receive("$ESC[?2004h".toByteArray(Charsets.UTF_8))
        rig.controller.paste("a\nb")
        assertEquals("$ESC[200~a\rb$ESC[201~", rig.socket.drainBinary())
    }

    @Test
    fun `an input burst over the server limit is chunked instead of dropped`() {
        val rig = Rig()
        rig.socket.binary.clear()

        val burst = "x".repeat(TERMINAL_INPUT_BYTES + 5)
        rig.controller.write(burst)

        assertEquals(2, rig.socket.binary.size)
        assertEquals(TERMINAL_INPUT_BYTES, rig.socket.binary[0].size)
        assertEquals(5, rig.socket.binary[1].size)
        assertEquals(burst, rig.socket.drainBinary())
    }

    @Test
    fun `close sends the close control frame and stops PTY writes`() {
        val rig = Rig()
        rig.socket.text.clear()
        rig.socket.binary.clear()

        rig.controller.session.finishIfRunning()

        assertEquals(listOf("""{"type":"close"}"""), rig.socket.text)
        rig.controller.write("ignored")
        assertTrue("a finished session writes nothing", rig.socket.binary.isEmpty())
    }

    @Test
    fun `an exit control frame ends the session locally`() {
        val rig = Rig()

        assertEquals(TerminalControl.Exit(3), parseTerminalControl("""{"type":"exit","status":3}"""))
        rig.controller.remoteExited(3)

        assertTrue("the controller reports the exit to its owner", rig.exited)
        rig.controller.write("ignored")
        assertTrue("input is dead after exit", rig.socket.binary.isEmpty())
    }

    @Test
    fun `server control frames parse into hello exit and error`() {
        assertEquals(
            TerminalControl.Hello(cols = 80, rows = 24, running = true),
            parseTerminalControl("""{"type":"hello","cols":80,"rows":24,"running":true}"""),
        )
        assertEquals(
            TerminalControl.Exit(null),
            parseTerminalControl("""{"type":"exit","status":null}"""),
        )
        assertEquals(
            TerminalControl.Failure("terminal is already attached"),
            parseTerminalControl("""{"type":"error","message":"terminal is already attached"}"""),
        )
        assertTrue(parseTerminalControl("""{"type":"screen"}""") is TerminalControl.Unreadable)
        assertTrue(parseTerminalControl("not json") is TerminalControl.Unreadable)
        assertTrue(parseTerminalControl("""["hello"]""") is TerminalControl.Unreadable)
    }

    @Test
    fun `reconnect backoff starts at 750 ms and stops at the ceiling`() {
        assertEquals(TERMINAL_RECONNECT_DELAY_MS, terminalReconnectDelayMs(0))
        assertEquals(TERMINAL_RECONNECT_DELAY_MS, terminalReconnectDelayMs(1))
        assertEquals(1_500L, terminalReconnectDelayMs(2))
        assertEquals(3_000L, terminalReconnectDelayMs(3))
        assertEquals(TERMINAL_RECONNECT_MAX_DELAY_MS, terminalReconnectDelayMs(4))
        assertEquals(TERMINAL_RECONNECT_MAX_DELAY_MS, terminalReconnectDelayMs(40))
    }

    @Test
    fun `capabilities treat a null archive kind as unsupported`() {
        val none = ApiJson.json.decodeFromString(
            Capabilities.serializer(),
            """{"spawn":false,"attach":true,"rename":true,"archive":null,"delete":false,"stop":true}""",
        )
        val reversible = ApiJson.json.decodeFromString(
            Capabilities.serializer(),
            """{"spawn":true,"attach":true,"rename":true,"archive":"Reversible","delete":false,"stop":true}""",
        )
        val deletes = ApiJson.json.decodeFromString(
            Capabilities.serializer(),
            """{"spawn":true,"attach":true,"rename":true,"archive":"Deletes","delete":true,"stop":true}""",
        )

        assertNull(none.archive)
        assertTrue(!none.canArchive)
        assertEquals("Reversible", reversible.archive)
        assertTrue(reversible.canArchive)
        assertEquals("Deletes", deletes.archive)
        assertTrue(deletes.canArchive)
    }

    private fun anyColoredCell(rig: Rig): Boolean {
        val emulator = rig.controller.view.mEmulator
        for (row in 0 until emulator.mRows) {
            for (col in 0 until emulator.mColumns) {
                val style = emulator.screen.getStyleAt(row, col)
                if (TextStyle.decodeForeColor(style) != TextStyle.COLOR_INDEX_FOREGROUND) return true
            }
        }
        return false
    }
}
