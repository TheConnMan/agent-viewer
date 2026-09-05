package com.theconnman.agentviewer

import android.content.Context
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.ViewGroup
import android.view.inputmethod.InputMethodManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import com.termux.terminal.RemoteTerminalSession
import com.termux.terminal.RemoteTerminalTransport
import com.termux.terminal.TerminalEmulator
import com.termux.terminal.TerminalSession
import com.termux.terminal.TerminalSessionClient
import com.termux.view.TerminalViewClient
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString

internal const val TERMINAL_ORIGIN = "https://appassets.androidplatform.net"

/** Server cap on a single inbound PTY write; the transport chunks anything larger. */
internal const val TERMINAL_INPUT_BYTES = 4_096

/** Rows of scrollback the phone-side emulator keeps. */
internal const val TERMINAL_TRANSCRIPT_ROWS = 2_000

/** Text size in pixels; 26 px is 67 columns at 1080 px portrait, measured in the spike. */
internal const val TERMINAL_TEXT_SIZE_PX = 26

internal const val TERMINAL_RECONNECT_DELAY_MS = 750L
internal const val TERMINAL_RECONNECT_MAX_DELAY_MS = 6_000L

/**
 * The server's takeover close code: a newer connection for this session now owns the PTY, and
 * this socket was closed to hand it over. Final for this console - reconnecting would only take
 * the session back off whichever device is now driving it.
 */
internal const val TERMINAL_CLOSE_REPLACED = 4_000

private val terminalHttpClient = OkHttpClient.Builder()
    .pingInterval(15, TimeUnit.SECONDS)
    .build()

// No default values: kotlinx.serialization drops defaults on encode, and the server keys
// every control frame off its type tag.
@Serializable
private data class TerminalResizeFrame(
    val type: String,
    val cols: Int,
    val rows: Int,
)

@Serializable
private data class TerminalCloseFrame(val type: String)

/** `{"type":"resize","cols":N,"rows":N}` exactly as the server's wire protocol spells it. */
internal fun terminalResizeFrame(columns: Int, rows: Int): String =
    ApiJson.json.encodeToString(
        TerminalResizeFrame.serializer(),
        TerminalResizeFrame(type = "resize", cols = columns, rows = rows),
    )

/** `{"type":"close"}`: kill the remote PTY now instead of leaving it in its grace window. */
internal fun terminalCloseFrame(): String =
    ApiJson.json.encodeToString(TerminalCloseFrame.serializer(), TerminalCloseFrame("close"))

/** The text (JSON control) frames the server can send. Binary frames are raw PTY output. */
internal sealed interface TerminalControl {
    data class Hello(val cols: Int, val rows: Int, val running: Boolean) : TerminalControl
    data class Exit(val status: Int?) : TerminalControl
    data class Failure(val message: String) : TerminalControl
    data class Unreadable(val reason: String) : TerminalControl
}

@Serializable
private data class TerminalHelloFrame(
    // The default Json is strict about unknown keys, so the tag is part of the shape.
    val type: String,
    val cols: Int,
    val rows: Int,
    val running: Boolean = true,
)

@Serializable
private data class TerminalExitFrame(
    val type: String,
    @SerialName("status") val status: Int? = null,
)

internal fun parseTerminalControl(text: String): TerminalControl = runCatching {
    val element = ApiJson.json.parseToJsonElement(text) as? JsonObject
        ?: return@runCatching TerminalControl.Unreadable("Terminal frame was not an object")
    val type = (element["type"] as? JsonPrimitive)?.takeIf { it.isString }?.content
        ?: return@runCatching TerminalControl.Unreadable("Terminal frame had no type")
    when (type) {
        "hello" -> ApiJson.json.decodeFromString(TerminalHelloFrame.serializer(), text)
            .let { TerminalControl.Hello(it.cols, it.rows, it.running) }

        "exit" -> ApiJson.json.decodeFromString(TerminalExitFrame.serializer(), text)
            .let { TerminalControl.Exit(it.status) }

        "error" -> TerminalControl.Failure(
            element["message"]?.jsonPrimitive?.content ?: "Terminal protocol error",
        )

        else -> TerminalControl.Unreadable("Unknown terminal frame: $type")
    }
}.getOrElse { TerminalControl.Unreadable(it.message ?: "Could not read terminal frame") }

/**
 * Everything the view wants to put on the PTY leaves as a binary WebSocket frame; geometry and
 * shutdown leave as the two JSON control frames. Nothing is inspected on the way out - the CLI's
 * replies to its own DA/DSR queries and mouse reports have to arrive verbatim.
 */
internal class WebSocketTerminalTransport(
    private val currentSocket: () -> WebSocket?,
) : RemoteTerminalTransport {

    override fun send(data: ByteArray, offset: Int, count: Int) {
        val socket = currentSocket() ?: return
        var start = offset
        var remaining = count
        while (remaining > 0) {
            val chunk = minOf(remaining, TERMINAL_INPUT_BYTES)
            socket.send(data.toByteString(start, chunk))
            start += chunk
            remaining -= chunk
        }
    }

    override fun resize(columns: Int, rows: Int) {
        currentSocket()?.send(terminalResizeFrame(columns, rows))
    }

    override fun close() {
        currentSocket()?.send(terminalCloseFrame())
    }
}

/**
 * Owns the Termux [com.termux.view.TerminalView] and its [RemoteTerminalSession] for the life of
 * one attached session. Sockets come and go underneath it; the emulator and its screen do not.
 */
internal class RemoteTerminalController(
    context: Context,
    private val transport: RemoteTerminalTransport,
    private val onExited: () -> Unit,
) {
    /** Sticky CTRL from the key row, consumed by the next key or IME character. */
    var stickyControl: Boolean = false

    private var announcedOnce = false

    val view: com.termux.view.TerminalView = com.termux.view.TerminalView(context, null)
    val session: RemoteTerminalSession

    private val sessionClient = object : TerminalSessionClient {
        override fun onTextChanged(changedSession: TerminalSession) = view.onScreenUpdated()
        override fun onTitleChanged(changedSession: TerminalSession) = Unit
        override fun onSessionFinished(finishedSession: TerminalSession) = onExited()
        override fun onCopyTextToClipboard(s: TerminalSession, text: String) = Unit
        override fun onPasteTextFromClipboard(s: TerminalSession?) = Unit
        override fun onBell(s: TerminalSession) = Unit
        override fun onColorsChanged(s: TerminalSession) = Unit
        override fun onTerminalCursorStateChange(state: Boolean) = Unit
        override fun getTerminalCursorStyle(): Int = TerminalEmulator.TERMINAL_CURSOR_STYLE_BLOCK
        override fun logError(tag: String, message: String) = Unit
        override fun logWarn(tag: String, message: String) = Unit
        override fun logInfo(tag: String, message: String) = Unit
        override fun logDebug(tag: String, message: String) = Unit
        override fun logVerbose(tag: String, message: String) = Unit
        override fun logStackTraceWithMessage(t: String, m: String, e: Exception) = Unit
        override fun logStackTrace(tag: String, e: Exception) = Unit
    }

    private val viewClient = object : TerminalViewClient {
        override fun onScale(scale: Float): Float = scale
        override fun onSingleTapUp(event: MotionEvent) = focusAndShowKeyboard()
        override fun shouldBackButtonBeMappedToEscape(): Boolean = false
        // Char based input: the vendored view answers the IME with
        // TYPE_TEXT_VARIATION_VISIBLE_PASSWORD | TYPE_TEXT_FLAG_NO_SUGGESTIONS, which is what
        // turns off autocorrect, suggestions, and glide typing. TYPE_NULL is the other branch,
        // and some keyboards do not reset their composing state on it.
        override fun shouldEnforceCharBasedInput(): Boolean = true
        override fun shouldUseCtrlSpaceWorkaround(): Boolean = false
        override fun isTerminalViewSelected(): Boolean = true
        override fun copyModeChanged(copyMode: Boolean) = Unit
        override fun onKeyDown(code: Int, e: KeyEvent, s: TerminalSession): Boolean = false
        override fun onKeyUp(code: Int, e: KeyEvent): Boolean = false
        override fun onLongPress(event: MotionEvent): Boolean = false

        override fun readControlKey(): Boolean {
            val pressed = stickyControl
            stickyControl = false
            return pressed
        }

        override fun readAltKey(): Boolean = false
        override fun readShiftKey(): Boolean = false
        override fun readFnKey(): Boolean = false
        override fun onCodePoint(point: Int, ctrlDown: Boolean, s: TerminalSession): Boolean = false

        override fun onEmulatorSet() {
            // Fires on every resize (keyboard show/hide); the first-attach announce runs once,
            // because every later size change already announces itself through updateSize.
            if (announcedOnce) return
            announcedOnce = true
            announceSize()
        }

        override fun logError(tag: String, message: String) = Unit
        override fun logWarn(tag: String, message: String) = Unit
        override fun logInfo(tag: String, message: String) = Unit
        override fun logDebug(tag: String, message: String) = Unit
        override fun logVerbose(tag: String, message: String) = Unit
        override fun logStackTraceWithMessage(t: String, m: String, e: Exception) = Unit
        override fun logStackTrace(tag: String, e: Exception) = Unit
    }

    init {
        // Termux sets these in its layout XML; a programmatic view is not focusable by default,
        // and an unfocusable view never receives key events or an IME connection.
        view.setFocusable(true)
        view.setFocusableInTouchMode(true)
        view.setTextSize(TERMINAL_TEXT_SIZE_PX)
        view.setTerminalViewClient(viewClient)
        view.layoutParams = ViewGroup.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.MATCH_PARENT,
        )
        session = RemoteTerminalSession(transport, TERMINAL_TRANSCRIPT_ROWS, sessionClient)
        view.attachSession(session)
        view.requestFocus()
    }

    /** Feed PTY output. Main thread only: it touches the emulator and the view. */
    fun receive(bytes: ByteArray) {
        session.receive(bytes, bytes.size)
    }

    /** Send bytes to the PTY; a no-op once the remote process is gone. */
    fun write(text: String) {
        session.write(text)
    }

    fun paste(text: String) {
        view.mEmulator?.paste(text)
    }

    /** Re-announce the current geometry to a freshly opened socket. */
    fun announceSize() {
        val emulator = view.mEmulator ?: return
        transport.resize(emulator.mColumns, emulator.mRows)
    }

    fun remoteExited(status: Int) {
        session.remoteExited(status)
    }

    fun focusAndShowKeyboard() {
        view.requestFocus()
        val ime = view.context.getSystemService(Context.INPUT_METHOD_SERVICE) as? InputMethodManager
        ime?.showSoftInput(view, 0)
    }

    fun detachFromParent() {
        (view.parent as? ViewGroup)?.removeView(view)
    }
}

/**
 * Connection state for one console: what the status line shows, whether a reconnect is owed, and
 * whether input is still live. A plain snapshot backed class rather than a pile of composable
 * locals, so the close and failure rules can be driven straight from a test.
 */
internal class TerminalConnection {
    var status by mutableStateOf("connecting")
    var geometry by mutableStateOf<String?>(null)
    var error by mutableStateOf<String?>(null)

    /** The remote process is gone. The screen stays readable; nothing reconnects. */
    var ended by mutableStateOf(false)
        private set

    /** Another connection took this session over, and this one is not getting it back. */
    var replaced by mutableStateOf(false)
        private set

    var failures by mutableStateOf(0)
        private set

    var reconnectScheduled by mutableStateOf(false)
    var closeRequested by mutableStateOf(false)

    /** No socket is opened again for this console once either terminal state is reached. */
    val finished: Boolean get() = ended || replaced

    fun opened() {
        failures = 0
        status = "connected"
        error = null
    }

    fun helloed(cols: Int, rows: Int, running: Boolean) {
        geometry = "${cols}x$rows"
        if (!running) sessionFinished()
    }

    fun sessionFinished() {
        ended = true
        status = "session ended"
    }

    /**
     * The server closed the socket. Returns true when the console should hand back to the list,
     * which is only ever the close this client asked for.
     *
     * [TERMINAL_CLOSE_REPLACED] is the takeover: another connection owns the PTY now, so a
     * reconnect would take the session off whatever is driving it. Every other code is a plain
     * drop and reconnects - including 1013, which the server sends when this client falls too
     * far behind the PTY, since the redraw on the next connect restores the whole screen.
     */
    fun closed(code: Int): Boolean {
        if (closeRequested) return true
        when {
            code == TERMINAL_CLOSE_REPLACED -> {
                replaced = true
                status = "replaced by another connection"
                reconnectScheduled = false
            }

            !ended -> {
                status = "disconnected"
                failures += 1
                reconnectScheduled = true
            }
        }
        return false
    }

    fun failed(message: String?) {
        if (finished) return
        status = "connection failed"
        error = message ?: "Could not connect to terminal"
        failures += 1
        reconnectScheduled = true
    }
}

/**
 * The socket callbacks for one connection attempt, kept out of the composable so a test can put
 * real close codes and real frames through them. [dispatch] is what hops to the main thread and
 * drops anything from an attempt that has already been superseded.
 */
internal fun terminalSocketListener(
    connection: TerminalConnection,
    controller: RemoteTerminalController,
    onSocket: (WebSocket?) -> Unit,
    onLeave: () -> Unit,
    dispatch: (() -> Unit) -> Unit,
): WebSocketListener = object : WebSocketListener() {

    override fun onOpen(webSocket: WebSocket, response: Response) = dispatch {
        onSocket(webSocket)
        connection.opened()
        controller.announceSize()
    }

    override fun onMessage(webSocket: WebSocket, bytes: ByteString) = dispatch {
        controller.receive(bytes.toByteArray())
    }

    override fun onMessage(webSocket: WebSocket, text: String) = dispatch {
        when (val control = parseTerminalControl(text)) {
            is TerminalControl.Hello -> connection.helloed(control.cols, control.rows, control.running)

            is TerminalControl.Exit -> {
                connection.sessionFinished()
                controller.remoteExited(control.status ?: 0)
            }

            is TerminalControl.Failure -> connection.error = control.message
            is TerminalControl.Unreadable -> connection.error = control.reason
        }
    }

    override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
        webSocket.close(code, reason)
    }

    override fun onClosed(webSocket: WebSocket, code: Int, reason: String) = dispatch {
        onSocket(null)
        if (connection.closed(code)) onLeave()
    }

    override fun onFailure(socket: WebSocket, cause: Throwable, response: Response?) = dispatch {
        onSocket(null)
        connection.failed(cause.message)
    }
}

/** 750 ms, doubling to a ceiling, so a server that stays down is not hammered. */
internal fun terminalReconnectDelayMs(consecutiveFailures: Int): Long {
    if (consecutiveFailures <= 1) return TERMINAL_RECONNECT_DELAY_MS
    val scaled = TERMINAL_RECONNECT_DELAY_MS shl (consecutiveFailures - 1).coerceAtMost(8)
    return scaled.coerceAtMost(TERMINAL_RECONNECT_MAX_DELAY_MS)
}

@Composable
internal fun TerminalConsole(
    session: Session,
    colors: ViewerColors,
    foreground: Boolean,
    onBack: () -> Unit,
) {
    val context = LocalContext.current
    val sessionKey = arrayOf(session.backend, session.id)
    // Only ever holds a socket that is open, and only the main thread writes it.
    val socketRef = remember(*sessionKey) { AtomicReference<WebSocket?>(null) }

    // Identifies the connection attempt a callback belongs to, so a socket that outlives its
    // effect cannot write into the screen state of the one that replaced it.
    val liveAttempt = remember(*sessionKey) { AtomicReference<Any?>(null) }
    val transport = remember(*sessionKey) { WebSocketTerminalTransport { socketRef.get() } }
    val connection = remember(*sessionKey) { TerminalConnection() }
    val controller = remember(*sessionKey) {
        RemoteTerminalController(context, transport) { connection.sessionFinished() }
    }
    var stickyControl by remember(*sessionKey) { mutableStateOf(false) }
    var attempt by remember(*sessionKey) { mutableStateOf(0L) }
    val scope = rememberCoroutineScope()

    BackHandler { onBack() }

    DisposableEffect(session.backend, session.id, foreground, attempt) {
        if (!foreground || connection.finished) {
            // Backgrounded: the socket goes, the session and its emulator stay. The server holds
            // the PTY for its grace window and repaints from its own screen on reconnect.
            if (!connection.finished) connection.status = "paused"
            onDispose { }
        } else {
            connection.status = if (attempt == 0L) "connecting" else "reconnecting"
            connection.reconnectScheduled = false
            connection.error = null
            val token = Any()
            liveAttempt.set(token)
            val request = Request.Builder()
                .url(terminalWebSocketUrl(ApiConfig.DEFAULT_BASE_URL, session.id, session.backend))
                .header("Origin", TERMINAL_ORIGIN)
                .build()
            val listener = terminalSocketListener(
                connection = connection,
                controller = controller,
                onSocket = { socketRef.set(it) },
                onLeave = onBack,
                // Hops to the main thread, then drops anything from a superseded attempt.
                dispatch = { action -> scope.launch { if (liveAttempt.get() === token) action() } },
            )
            val openedSocket = terminalHttpClient.newWebSocket(request, listener)
            onDispose {
                if (liveAttempt.compareAndSet(token, null)) socketRef.set(null)
                openedSocket.close(1000, "console detached")
            }
        }
    }

    LaunchedEffect(foreground, connection.reconnectScheduled, connection.finished) {
        if (foreground && connection.reconnectScheduled && !connection.finished) {
            delay(terminalReconnectDelayMs(connection.failures))
            if (foreground && connection.reconnectScheduled && !connection.finished) attempt += 1L
        }
    }

    DisposableEffect(controller) {
        onDispose { controller.detachFromParent() }
    }

    val liveInput = !connection.finished && !connection.closeRequested

    Column(modifier = Modifier.fillMaxSize()) {
        Row(
            modifier = Modifier.fillMaxWidth().height(60.dp).padding(horizontal = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            TextButton(onClick = onBack) { Text("back", color = colors.muted) }
            BackendIcon(session.backend, colors, size = 15.dp)
            Spacer(Modifier.width(8.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    session.title,
                    color = colors.text,
                    fontSize = 15.sp,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    listOfNotNull(
                        "terminal",
                        connection.status,
                        connection.geometry,
                    ).joinToString("  "),
                    color = if (connection.status == "connected") colors.ok else colors.faint,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 10.sp,
                )
            }
            TextButton(
                onClick = {
                    connection.closeRequested = true
                    connection.status = "closing"
                    if (socketRef.get()?.send(terminalCloseFrame()) != true) {
                        connection.closeRequested = false
                        connection.error = "Could not request console close"
                    }
                },
                enabled = liveInput,
            ) { Text("close console", color = colors.err) }
        }

        connection.error?.let {
            Text(
                it,
                color = colors.err,
                fontFamily = FontFamily.Monospace,
                fontSize = 11.sp,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp),
            )
        }

        Box(modifier = Modifier.fillMaxWidth().weight(1f).background(colors.surface)) {
            AndroidView(
                factory = {
                    controller.detachFromParent()
                    controller.view
                },
                modifier = Modifier.fillMaxSize(),
            )
        }

        Row(
            modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState())
                .padding(horizontal = 10.dp, vertical = 8.dp),
            horizontalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            TerminalKey("enter", colors, liveInput) { controller.write("\r") }
            TerminalKey("esc", colors, liveInput) { controller.write("\u001b") }
            TerminalKey("tab", colors, liveInput) { controller.write("\t") }
            TerminalKey(
                label = if (stickyControl) "[ctrl]" else "ctrl",
                colors = colors,
                enabled = liveInput,
            ) {
                stickyControl = !stickyControl
                controller.stickyControl = stickyControl
            }
            TerminalKey("up", colors, liveInput) { controller.write("\u001b[A") }
            TerminalKey("down", colors, liveInput) { controller.write("\u001b[B") }
            TerminalKey("left", colors, liveInput) { controller.write("\u001b[D") }
            TerminalKey("right", colors, liveInput) { controller.write("\u001b[C") }
            TerminalKey("ctrl c", colors, liveInput) { controller.write("\u0003") }
            TerminalKey("paste", colors, liveInput) {
                terminalClipboardText(context)?.let(controller::paste)
            }
            TerminalKey("keyboard", colors, liveInput) { controller.focusAndShowKeyboard() }
        }
    }
}

private fun terminalClipboardText(context: Context): String? {
    val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE)
        as? android.content.ClipboardManager ?: return null
    return clipboard.primaryClip
        ?.takeIf { it.itemCount > 0 }
        ?.getItemAt(0)
        ?.coerceToText(context)
        ?.toString()
        ?.takeIf { it.isNotEmpty() }
}

@Composable
private fun TerminalKey(label: String, colors: ViewerColors, enabled: Boolean, onClick: () -> Unit) {
    OutlinedButton(
        onClick = onClick,
        enabled = enabled,
        contentPadding = PaddingValues(horizontal = 11.dp, vertical = 7.dp),
    ) {
        Text(
            label,
            color = if (enabled) colors.accent else colors.faint,
            fontFamily = FontFamily.Monospace,
            fontSize = 10.sp,
        )
    }
}
