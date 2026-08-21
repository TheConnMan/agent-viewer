package com.theconnman.agentviewer

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
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
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch
import kotlinx.coroutines.delay
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import java.util.concurrent.TimeUnit

private const val TERMINAL_ORIGIN = "https://appassets.androidplatform.net"
private const val TERMINAL_ROWS = 32
private const val TERMINAL_COLS = 56
private const val TERMINAL_INPUT_LIMIT = 4_096
private val TERMINAL_CELL_WIDTH = 6.7.dp
private val TERMINAL_CELL_HEIGHT = 15.dp
private val terminalHttpClient = OkHttpClient.Builder()
    .pingInterval(15, TimeUnit.SECONDS)
    .build()

internal data class TerminalInputState(
    val draft: String = "",
    val pendingId: Long? = null,
) {
    fun enqueued(id: Long): TerminalInputState = copy(pendingId = id)

    fun acknowledged(id: Long): TerminalInputState =
        if (pendingId == id) TerminalInputState() else this

    fun failed(): TerminalInputState = copy(pendingId = null)
}

internal data class TerminalConnectionState(
    val generation: Long = 0L,
    val inputState: TerminalInputState = TerminalInputState(),
    val screenSequence: Long? = null,
    val reconnectScheduled: Boolean = false,
)

internal enum class TerminalSocketEnd {
    SPONTANEOUS_CLOSE,
    FAILURE,
    INTENTIONAL_CLOSE,
    DISPOSE,
}

internal fun terminalConnectionEnded(
    state: TerminalConnectionState,
    callbackGeneration: Long,
    ending: TerminalSocketEnd,
    screenRunning: Boolean?,
): TerminalConnectionState {
    if (callbackGeneration != state.generation) return state
    return state.copy(
        inputState = state.inputState.failed(),
        reconnectScheduled = screenRunning != false &&
            (ending == TerminalSocketEnd.SPONTANEOUS_CLOSE || ending == TerminalSocketEnd.FAILURE),
    )
}

internal data class TerminalSocketAttempt(
    val connectionState: TerminalConnectionState,
    val clearScreen: Boolean,
)

internal fun terminalSocketAttemptStarted(state: TerminalConnectionState): TerminalSocketAttempt =
    TerminalSocketAttempt(
        connectionState = state.copy(
            generation = state.generation + 1L,
            screenSequence = null,
            reconnectScheduled = false,
        ),
        clearScreen = true,
    )

internal fun terminalScreenFrameIsCurrent(
    state: TerminalConnectionState,
    callbackGeneration: Long,
    sequence: Long,
): Boolean = callbackGeneration == state.generation &&
    sequence >= (state.screenSequence ?: Long.MIN_VALUE)

internal fun terminalInputAfterServerFrame(
    state: TerminalInputState,
    frame: String,
): TerminalInputState = runCatching {
    when (
        ApiJson.json.parseToJsonElement(frame)
            .jsonObject.getValue("type").jsonPrimitive.content
    ) {
        "ack" -> state.acknowledged(ApiJson.json.decodeFromString<TerminalAck>(frame).id)
        "screen" -> ApiJson.json.decodeFromString<TerminalScreen>(frame).let { state }
        else -> state.failed()
    }
}.getOrElse { state.failed() }

internal fun terminalLineCommand(id: Long, draft: String): TerminalCommand.Submit? {
    if (draft.isEmpty()) return null
    return draft.takeIf { it.toByteArray(Charsets.UTF_8).size <= TERMINAL_INPUT_LIMIT }
        ?.let { TerminalCommand.Submit(id, it) }
}

internal fun terminalSubmitEnabled(
    connectionState: String,
    screen: TerminalScreen?,
    pendingId: Long?,
    closeRequested: Boolean,
): Boolean = connectionState == "connected" &&
    screen?.running == true &&
    screen.ready &&
    pendingId == null &&
    !closeRequested

internal data class TerminalRenderFrame(
    val text: String,
    val cursorRow: Int,
    val cursorCol: Int,
)

internal fun terminalRenderFrame(screen: TerminalScreen): TerminalRenderFrame = TerminalRenderFrame(
    text = screen.lines.joinToString("\n"),
    cursorRow = screen.cursorRow,
    cursorCol = screen.cursorCol,
)

internal enum class TerminalExit { BACK, DISPOSE, CLOSE }

internal fun terminalExitCommand(exit: TerminalExit): TerminalCommand? = when (exit) {
    TerminalExit.BACK, TerminalExit.DISPOSE -> null
    TerminalExit.CLOSE -> TerminalCommand.Close
}

@Composable
internal fun TerminalConsole(
    session: Session,
    colors: ViewerColors,
    foreground: Boolean,
    onBack: () -> Unit,
) {
    var socket by remember(session.backend, session.id) { mutableStateOf<WebSocket?>(null) }
    var screen by remember(session.backend, session.id) { mutableStateOf<TerminalScreen?>(null) }
    var connectionState by remember(session.backend, session.id) { mutableStateOf("connecting") }
    var error by remember(session.backend, session.id) { mutableStateOf<String?>(null) }
    var terminalState by remember(session.backend, session.id) { mutableStateOf(TerminalConnectionState()) }
    var socketAttemptNonce by remember(session.backend, session.id) { mutableStateOf(0L) }
    var nextSubmitId by remember(session.backend, session.id) { mutableStateOf(1L) }
    var closeRequested by remember(session.backend, session.id) { mutableStateOf(false) }
    val scope = rememberCoroutineScope()
    val verticalScroll = rememberScrollState()
    val horizontalScroll = rememberScrollState()

    fun send(command: TerminalCommand): Boolean {
        val encoded = ApiJson.json.encodeToString<TerminalCommand>(command)
        val sent = socket?.send(encoded) == true
        if (!sent) error = "Terminal is not connected"
        return sent
    }

    fun sendLine() {
        val inputState = terminalState.inputState
        if (!terminalSubmitEnabled(connectionState, screen, inputState.pendingId, closeRequested)) return
        val command = terminalLineCommand(nextSubmitId, inputState.draft)
        if (command == null) {
            if (inputState.draft.isNotEmpty()) error = "Input exceeds the 4096 byte limit"
            return
        }
        if (send(command)) {
            terminalState = terminalState.copy(inputState = inputState.enqueued(command.id))
            nextSubmitId += 1
            error = null
        }
    }

    BackHandler {
        terminalExitCommand(TerminalExit.BACK)?.let(::send)
        onBack()
    }

    DisposableEffect(session.backend, session.id, foreground, socketAttemptNonce) {
        if (!foreground) {
            val callbackGeneration = terminalState.generation
            connectionState = "paused"
            socket = null
            terminalState = terminalConnectionEnded(
                terminalState,
                callbackGeneration,
                TerminalSocketEnd.DISPOSE,
                screen?.running,
            )
            onDispose { }
        } else {
            val attempt = terminalSocketAttemptStarted(terminalState)
            terminalState = attempt.connectionState
            if (attempt.clearScreen) screen = null
            val callbackGeneration = attempt.connectionState.generation
            connectionState = "connecting"
            error = null
            val request = Request.Builder()
                .url(terminalWebSocketUrl(ApiConfig.DEFAULT_BASE_URL, session.id, session.backend))
                .header("Origin", TERMINAL_ORIGIN)
                .build()
            lateinit var openedSocket: WebSocket
            val listener = object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    scope.launch {
                        if (socket === openedSocket && terminalState.generation == callbackGeneration) {
                            connectionState = if (screen?.running == false) "session ended" else "connected"
                            error = null
                            send(TerminalCommand.Resize(TERMINAL_ROWS, TERMINAL_COLS))
                        }
                    }
                }

                override fun onMessage(webSocket: WebSocket, text: String) {
                    scope.launch {
                        if (socket !== openedSocket || terminalState.generation != callbackGeneration) {
                            return@launch
                        }
                        terminalState = terminalState.copy(
                            inputState = terminalInputAfterServerFrame(terminalState.inputState, text),
                        )
                        runCatching {
                            val messageType = ApiJson.json.parseToJsonElement(text)
                                .jsonObject.getValue("type").jsonPrimitive.content
                            when (messageType) {
                                "screen" -> {
                                    val next = ApiJson.json.decodeFromString<TerminalScreen>(text)
                                    if (terminalScreenFrameIsCurrent(terminalState, callbackGeneration, next.seq)) {
                                        screen = next
                                        terminalState = terminalState.copy(screenSequence = next.seq)
                                        connectionState = if (next.running) "connected" else "session ended"
                                    }
                                }
                                "ack" -> ApiJson.json.decodeFromString<TerminalAck>(text)
                                "error" -> {
                                    error = ApiJson.json.parseToJsonElement(text)
                                        .jsonObject["message"]?.jsonPrimitive?.content
                                        ?: "Terminal protocol error"
                                }
                                else -> error = "Unknown terminal message"
                            }
                        }.onFailure { failure ->
                            error = failure.message ?: "Could not read terminal message"
                        }
                    }
                }

                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                    webSocket.close(code, reason)
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    scope.launch {
                        if (socket === openedSocket) {
                            terminalState = terminalConnectionEnded(
                                terminalState,
                                callbackGeneration,
                                if (closeRequested) {
                                    TerminalSocketEnd.INTENTIONAL_CLOSE
                                } else {
                                    TerminalSocketEnd.SPONTANEOUS_CLOSE
                                },
                                screen?.running,
                            )
                            if (closeRequested) {
                                onBack()
                            } else if (screen?.running != false) {
                                connectionState = "disconnected"
                            }
                        }
                    }
                }

                override fun onFailure(webSocket: WebSocket, failure: Throwable, response: Response?) {
                    scope.launch {
                        if (socket === openedSocket) {
                            terminalState = terminalConnectionEnded(
                                terminalState,
                                callbackGeneration,
                                if (closeRequested) {
                                    TerminalSocketEnd.INTENTIONAL_CLOSE
                                } else {
                                    TerminalSocketEnd.FAILURE
                                },
                                screen?.running,
                            )
                            if (screen?.running != false) connectionState = "connection failed"
                            error = failure.message ?: "Could not connect to terminal"
                        }
                    }
                }
            }
            openedSocket = terminalHttpClient.newWebSocket(request, listener)
            socket = openedSocket
            onDispose {
                if (socket === openedSocket) socket = null
                terminalState = terminalConnectionEnded(
                    terminalState,
                    callbackGeneration,
                    TerminalSocketEnd.DISPOSE,
                    screen?.running,
                )
                terminalExitCommand(TerminalExit.DISPOSE)?.let { command ->
                    openedSocket.send(ApiJson.json.encodeToString<TerminalCommand>(command))
                }
                openedSocket.close(1000, "console left")
            }
        }
    }

    LaunchedEffect(foreground, terminalState.reconnectScheduled) {
        if (foreground && terminalState.reconnectScheduled) {
            val scheduledGeneration = terminalState.generation
            delay(750)
            if (
                foreground &&
                terminalState.reconnectScheduled &&
                terminalState.generation == scheduledGeneration
            ) {
                connectionState = "reconnecting"
                socketAttemptNonce += 1L
            }
        }
    }

    LaunchedEffect(screen?.seq) {
        if (screen != null) {
            withFrameNanos { }
            verticalScroll.scrollTo(verticalScroll.maxValue)
        }
    }

    val activeScreen = screen
    val inputState = terminalState.inputState
    val inputEnabled = activeScreen?.running == true && connectionState == "connected" && !closeRequested
    val submitEnabled = terminalSubmitEnabled(
        connectionState,
        activeScreen,
        inputState.pendingId,
        closeRequested,
    )

    Column(modifier = Modifier.fillMaxSize()) {
        Row(
            modifier = Modifier.fillMaxWidth().height(60.dp).padding(horizontal = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            TextButton(onClick = onBack) { Text("back", color = colors.muted) }
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    session.title,
                    color = colors.text,
                    fontSize = 15.sp,
                    fontWeight = FontWeight.SemiBold,
                    maxLines = 1,
                )
                Text(
                    "${terminalBackendTag(session.backend)} terminal  $connectionState",
                    color = if (connectionState == "connected") colors.ok else colors.faint,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 10.sp,
                )
            }
            TextButton(
                onClick = {
                    val closeCommand = terminalExitCommand(TerminalExit.CLOSE)
                    closeRequested = true
                    if (closeCommand == null || !send(closeCommand)) {
                        closeRequested = false
                        error = "Could not request console close"
                    } else {
                        connectionState = "closing"
                    }
                },
                enabled = !closeRequested,
            ) { Text("close console", color = colors.err) }
        }

        error?.let {
            Text(
                it,
                color = colors.err,
                fontFamily = FontFamily.Monospace,
                fontSize = 11.sp,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp),
            )
        }

        Box(
            modifier = Modifier.fillMaxWidth().weight(1f).background(colors.surface)
                .padding(horizontal = 10.dp, vertical = 8.dp)
                .horizontalScroll(horizontalScroll)
                .verticalScroll(verticalScroll),
        ) {
            if (activeScreen == null) {
                Text(
                    text = "Waiting for terminal screen",
                    color = colors.text,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 11.sp,
                )
            } else {
                TerminalFrame(terminalRenderFrame(activeScreen), activeScreen, colors)
            }
        }

        Row(
            modifier = Modifier.fillMaxWidth().padding(start = 10.dp, end = 10.dp, top = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            OutlinedTextField(
                value = inputState.draft,
                onValueChange = {
                    terminalState = terminalState.copy(inputState = inputState.copy(draft = it))
                },
                modifier = Modifier.weight(1f),
                enabled = submitEnabled,
                singleLine = true,
                placeholder = { Text("type into session", color = colors.faint) },
                textStyle = androidx.compose.ui.text.TextStyle(
                    color = colors.text,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 12.sp,
                ),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                keyboardActions = KeyboardActions(onSend = { sendLine() }),
            )
            Spacer(Modifier.width(8.dp))
            Button(
                onClick = { sendLine() },
                enabled = submitEnabled && inputState.draft.isNotEmpty(),
            ) { Text("send") }
        }

        Row(
            modifier = Modifier.fillMaxWidth().horizontalScroll(rememberScrollState())
                .padding(horizontal = 10.dp, vertical = 8.dp),
            horizontalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            TerminalKey("enter", colors, inputEnabled) { send(TerminalCommand.Input("\r")) }
            TerminalKey("backspace", colors, inputEnabled) { send(TerminalCommand.Input("\u007f")) }
            TerminalKey("tab", colors, inputEnabled) { send(TerminalCommand.Input("\t")) }
            TerminalKey("escape", colors, inputEnabled) { send(TerminalCommand.Input("\u001b")) }
            TerminalKey("up", colors, inputEnabled) { send(TerminalCommand.Input("\u001b[A")) }
            TerminalKey("down", colors, inputEnabled) { send(TerminalCommand.Input("\u001b[B")) }
            TerminalKey("left", colors, inputEnabled) { send(TerminalCommand.Input("\u001b[D")) }
            TerminalKey("right", colors, inputEnabled) { send(TerminalCommand.Input("\u001b[C")) }
            TerminalKey("ctrl c", colors, inputEnabled) { send(TerminalCommand.Input("\u0003")) }
        }
    }
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

@Composable
private fun TerminalFrame(frame: TerminalRenderFrame, screen: TerminalScreen, colors: ViewerColors) {
    Box(
        modifier = Modifier
            .width(terminalExtent(screen.cols, TERMINAL_CELL_WIDTH))
            .height(terminalExtent(screen.rows, TERMINAL_CELL_HEIGHT)),
    ) {
        Text(
            text = frame.text,
            color = colors.text,
            fontFamily = FontFamily.Monospace,
            fontSize = 11.sp,
            lineHeight = 15.sp,
            softWrap = false,
        )
        if (frame.cursorRow in 0 until screen.rows && frame.cursorCol in 0 until screen.cols) {
            Text(
                text = "█",
                color = colors.accent,
                fontFamily = FontFamily.Monospace,
                fontSize = 11.sp,
                lineHeight = 15.sp,
                modifier = Modifier.alpha(0.75f).offset {
                    IntOffset(
                        x = (frame.cursorCol * TERMINAL_CELL_WIDTH.toPx()).toInt(),
                        y = (frame.cursorRow * TERMINAL_CELL_HEIGHT.toPx()).toInt(),
                    )
                },
            )
        }
    }
}

private fun terminalExtent(cells: Int, cellSize: Dp): Dp = cellSize * cells.coerceAtLeast(1)

private fun terminalBackendTag(backend: BackendKind): String = when (backend) {
    BackendKind.CODEX -> "CX"
    BackendKind.CLAUDE -> "CC"
}
