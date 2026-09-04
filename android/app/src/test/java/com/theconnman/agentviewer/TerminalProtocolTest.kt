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
 * The client half of the terminal wire protocol, driven through the real Termux view and the real
 * WebSocket transport. Stubbing [WebSocketTerminalTransport] out fails every assertion here: the
 * proof is bytes and JSON arriving at a fake socket, not calls being made.
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

    private class Rig {
        val socket = FakeWebSocket()
        val transport = WebSocketTerminalTransport { socket }
        var exited = false
        val controller: RemoteTerminalController

        init {
            val activity = Robolectric.buildActivity(Activity::class.java).setup().get()
            controller = RemoteTerminalController(activity, transport) { exited = true }
            activity.setContentView(controller.view)
            controller.view.measure(
                View.MeasureSpec.makeMeasureSpec(PHONE_W, View.MeasureSpec.EXACTLY),
                View.MeasureSpec.makeMeasureSpec(PHONE_H, View.MeasureSpec.EXACTLY),
            )
            controller.view.layout(0, 0, PHONE_W, PHONE_H)
        }

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
