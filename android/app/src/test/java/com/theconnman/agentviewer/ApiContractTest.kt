package com.theconnman.agentviewer

import java.io.BufferedInputStream
import java.io.ByteArrayOutputStream
import java.io.Closeable
import java.net.InetAddress
import java.net.ServerSocket
import java.net.SocketException
import java.nio.charset.StandardCharsets.UTF_8
import java.util.concurrent.CopyOnWriteArrayList
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertFailsWith
import kotlin.test.assertNull
import kotlin.test.assertTrue
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.SerializationException
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonObject

class ApiContractTest {
    @Test
    fun `decodes a session with a needs input reason and project root`() {
        val session = ApiJson.json.decodeFromString<Session>(
            """
            {
              "backend": "Codex",
              "id": "thread-123",
              "short_id": null,
              "origin": "Background",
              "title": "Investigate phone companion",
              "cwd": "/home/brian/git/agent-viewer/.worktrees/mobile-companion",
              "git_branch": "task/mobile-companion",
              "status": { "NeedsInput": { "reason": "Approve the design" } },
              "created_at_ms": 1724150000000,
              "updated_at_ms": 1724151000000,
              "hidden": false,
              "companion": false,
              "subagent": false,
              "summary": "Waiting for approval",
              "pid": 1234,
              "rollout_path": "/home/brian/.codex/rollouts/thread-123.jsonl",
              "pr_refs": [{ "id": "42", "href": "https://github.com/TheConnMan/agent-viewer/pull/42" }],
              "daemon_hosted": true,
              "project_root": "/home/brian/git/agent-viewer"
            }
            """.trimIndent(),
        )

        assertEquals(BackendKind.CODEX, session.backend)
        assertEquals("thread-123", session.id)
        assertEquals("/home/brian/git/agent-viewer", session.projectRoot)
        assertEquals("Approve the design", (session.status as Status.NeedsInput).reason)
    }

    @Test
    fun `decodes serde externally tagged tail events`() {
        val agent = ApiJson.json.decodeFromString<TailEvent>("{\"Agent\":\"I will inspect the logs\"}")
        val user = ApiJson.json.decodeFromString<TailEvent>("{\"User\":\"Please investigate\"}")
        val tool = ApiJson.json.decodeFromString<TailEvent>(
            "{\"Tool\":{\"name\":\"Bash\",\"detail\":\"rg --files\"}}",
        )

        assertEquals("I will inspect the logs", (agent as TailEvent.Agent).text)
        assertEquals("Please investigate", (user as TailEvent.User).text)
        assertEquals("Bash", (tool as TailEvent.Tool).name)
        assertEquals("rg --files", tool.detail)
    }

    @Test
    fun `uses the tailnet server as the default API base URL`() {
        assertEquals("https://remote-dev.tail26e2ce.ts.net:8443", ApiConfig.DEFAULT_BASE_URL)
    }

    @Test
    fun `ships the four viewer themes with analog amber selected by default`() {
        assertEquals(
            listOf("analog amber", "dracula", "tokyo night", "paper light"),
            AppTheme.entries.map(AppTheme::displayName),
        )
        assertEquals(AppTheme.ANALOG_AMBER, AppTheme.default)
    }

    @Test
    fun `composer supports exactly the provider catalog without effort`() {
        assertEquals(
            listOf("auto", "claude", "codex"),
            ComposerProvider.entries.map(ComposerProvider::wireName),
        )

        val request = NewSessionRequest(
            provider = ComposerProvider.AUTO,
            model = "default",
            folder = "/home/brian/git/agent-viewer",
            task = "Start the companion server",
        )
        val encoded = ApiJson.json.encodeToString(request)

        assertTrue(encoded.contains("\"provider\":\"auto\""))
        assertFalse(encoded.contains("effort"))
    }

    @Test
    fun `mutations post empty JSON to the exact session action`() = withServer { client, requests ->
        runBlocking {
            client.stop("thread-123")
            client.hide("thread-123", hidden = false)
            client.hide("thread-123", hidden = true)
        }

        assertEquals(
            listOf(
                CapturedRequest("POST", "/sessions/thread-123/stop", "application/json", "{}"),
                CapturedRequest("POST", "/sessions/thread-123/hide", "application/json", "{}"),
                CapturedRequest("POST", "/sessions/thread-123/unhide", "application/json", "{}"),
            ),
            requests,
        )
    }

    @Test
    fun `models reject non string catalog entries`() = withServer(
        responseBody = """{"Codex":["gpt-5.6-sol",7],"Claude":[]}""",
    ) { client, _ ->
        assertFailsWith<SerializationException> {
            runBlocking { client.models() }
        }
    }

    @Test
    fun `models require both backend catalogs`() {
        listOf(
            """{"Codex":[]}""",
            """{"Claude":[]}""",
        ).forEach { response ->
            withServer(responseBody = response) { client, _ ->
                assertFailsWith<SerializationException> {
                    runBlocking { client.models() }
                }
            }
        }
    }

    @Test
    fun `session submitter rejects an overlapping submission`() = runBlocking {
        val createStarted = CompletableDeferred<Unit>()
        val releaseCreate = CompletableDeferred<Unit>()
        var createCalls = 0
        val submitter = SessionSubmitter {
            createCalls += 1
            createStarted.complete(Unit)
            releaseCreate.await()
        }
        val request = NewSessionRequest(
            provider = ComposerProvider.AUTO,
            model = "auto",
            folder = "/tmp/project",
            task = "Start the session",
        )

        val first = async { submitter.submit(request) }
        createStarted.await()

        assertNull(submitter.submit(request))
        assertEquals(1, createCalls)

        releaseCreate.complete(Unit)
        assertTrue(first.await()?.isSuccess == true)
    }

    @Test
    fun `session row helpers enforce companion and visibility boundaries`() {
        val visibleWorking = session("visible-working", Status.Working)
        val visibleWaiting = session("visible-waiting", Status.NeedsInput("Approve it"))
        val hiddenWaiting = session("hidden-waiting", Status.NeedsInput(null), hidden = true)
        val companionWaiting = session("companion-waiting", Status.NeedsInput("Internal"), companion = true)
        val hiddenCompanion = session("hidden-companion", Status.Done, hidden = true, companion = true)
        val sessions = listOf(
            visibleWorking,
            visibleWaiting,
            hiddenWaiting,
            companionWaiting,
            hiddenCompanion,
        )

        assertEquals(
            listOf("visible-working", "visible-waiting"),
            normalSessionRows(sessions).map(Session::id),
        )
        assertEquals(
            listOf("hidden-waiting", "companion-waiting", "hidden-companion"),
            hiddenSessionRows(sessions).map(Session::id),
        )
        assertEquals(
            listOf("visible-waiting"),
            triageSessionRows(sessions).map(Session::id),
        )
    }

    @Test
    fun `session row helpers omit hold titles without hiding longer titles`() {
        val visibleHold = session("visibleHold", Status.Working, title = "Hold")
        val visibleMixedHold = session("visibleMixedHold", Status.Working, title = "hOlD")
        val visibleLongerTitle = session("visibleLongerTitle", Status.Working, title = "hold task")
        val hiddenHold = session("hiddenHold", Status.Done, hidden = true, title = "Hold")
        val hiddenMixedHold = session("hiddenMixedHold", Status.Done, hidden = true, title = "hOlD")
        val hiddenLongerTitle = session("hiddenLongerTitle", Status.Done, hidden = true, title = "hold task")
        val triageHold = session("triageHold", Status.NeedsInput(null), title = "Hold")
        val triageMixedHold = session("triageMixedHold", Status.NeedsInput(null), title = "hOlD")
        val triageLongerTitle = session("triageLongerTitle", Status.NeedsInput(null), title = "hold task")
        val sessions = listOf(
            visibleHold,
            visibleMixedHold,
            visibleLongerTitle,
            hiddenHold,
            hiddenMixedHold,
            hiddenLongerTitle,
            triageHold,
            triageMixedHold,
            triageLongerTitle,
        )

        assertEquals(
            listOf("visibleLongerTitle", "triageLongerTitle"),
            normalSessionRows(sessions).map(Session::id),
        )
        assertEquals(
            listOf("hiddenLongerTitle"),
            hiddenSessionRows(sessions).map(Session::id),
        )
        assertEquals(
            listOf("triageLongerTitle"),
            triageSessionRows(sessions).map(Session::id),
        )
    }

    @Test
    fun `tail bottom index selects the sentinel after the newest event and handles an empty tail`() {
        val tail = listOf(
            TailEvent.User("First"),
            TailEvent.Tool("Bash", "pwd"),
            TailEvent.Agent("Newest"),
        )

        assertEquals(3, tailBottomIndex(tail))
        assertNull(tailBottomIndex(emptyList()))
    }

    @Test
    fun `terminal websocket URL uses a secure encoded session path`() {
        assertEquals(
            "wss://remote-dev.tail26e2ce.ts.net:8443/sessions/thread%2Fwith%20space%3F%23/terminal?backend=Codex",
            terminalWebSocketUrl(
                ApiConfig.DEFAULT_BASE_URL,
                "thread/with space?#",
                BackendKind.CODEX,
            ),
        )
        assertEquals(
            "ws://127.0.0.1:8477/sessions/local%20session/terminal?backend=Claude",
            terminalWebSocketUrl(
                "http://127.0.0.1:8477/",
                "local session",
                BackendKind.CLAUDE,
            ),
        )
    }

    @Test
    fun `terminal screen message decodes the complete screen contract`() {
        val screen = ApiJson.json.decodeFromString<TerminalScreen>(
            """
            {
              "type": "screen",
              "seq": 42,
              "rows": 24,
              "cols": 80,
              "cursor_row": 23,
              "cursor_col": 7,
              "lines": ["first line", "waiting for input"],
              "running": true,
              "ready": true
            }
            """.trimIndent(),
        )

        assertEquals("screen", screen.type)
        assertEquals(42, screen.seq)
        assertEquals(24, screen.rows)
        assertEquals(80, screen.cols)
        assertEquals(23, screen.cursorRow)
        assertEquals(7, screen.cursorCol)
        assertEquals(listOf("first line", "waiting for input"), screen.lines)
        assertTrue(screen.running)
        assertTrue(screen.ready)
    }

    @Test
    fun `terminal commands serialize Unicode input and resize dimensions`() {
        val input = ApiJson.json.parseToJsonElement(
            ApiJson.json.encodeToString<TerminalCommand>(TerminalCommand.Input("Review café 漢字")),
        ).jsonObject
        val resize = ApiJson.json.parseToJsonElement(
            ApiJson.json.encodeToString<TerminalCommand>(TerminalCommand.Resize(rows = 32, cols = 56)),
        ).jsonObject

        assertEquals(JsonPrimitive("input"), input["type"])
        assertEquals(JsonPrimitive("Review café 漢字"), input["text"])
        assertEquals(JsonPrimitive("resize"), resize["type"])
        assertEquals(JsonPrimitive(32), resize["rows"])
        assertEquals(JsonPrimitive(56), resize["cols"])
    }

    @Test
    fun `terminal submit separates line text from enter and carries a delivery id`() {
        val deliveryId = 5_000_000_000L
        val submit = ApiJson.json.parseToJsonElement(
            ApiJson.json.encodeToString<TerminalCommand>(
                TerminalCommand.Submit(id = deliveryId, text = "Review café 漢字"),
            ),
        ).jsonObject

        assertEquals(JsonPrimitive("submit"), submit["type"])
        assertEquals(JsonPrimitive(deliveryId), submit["id"])
        assertEquals(JsonPrimitive("Review café 漢字"), submit["text"])
        assertEquals(setOf("type", "id", "text"), submit.keys)
    }

    @Test
    fun `terminal acknowledgement clears only its matching pending draft`() {
        val deliveryId = 5_000_000_000L
        val queued = TerminalInputState(draft = "keep this message").enqueued(deliveryId)
        val unrelatedAck = terminalInputAfterServerFrame(
            queued,
            """{"type":"ack","id":4999999999}""",
        )
        val matchingAck = terminalInputAfterServerFrame(
            queued,
            """{"type":"ack","id":$deliveryId}""",
        )
        val failed = queued.failed()
        val ack = ApiJson.json.decodeFromString<TerminalAck>(
            """{"type":"ack","id":$deliveryId}""",
        )

        assertEquals("keep this message", queued.draft)
        assertEquals(deliveryId, queued.pendingId)
        assertEquals(queued, unrelatedAck)
        assertEquals("", matchingAck.draft)
        assertNull(matchingAck.pendingId)
        assertEquals("keep this message", failed.draft)
        assertNull(failed.pendingId)
        assertEquals("ack", ack.type)
        assertEquals(deliveryId, ack.id)
    }

    @Test
    fun `terminal malformed and unknown frames preserve the draft for retry`() {
        val queued = TerminalInputState(draft = "retry this message").enqueued(5_000_000_000L)

        listOf(
            """{"type":"ack","id":"not a number"}""",
            """{"type":"future-frame"}""",
            "not json",
        ).forEach { frame ->
            val retryable = terminalInputAfterServerFrame(queued, frame)
            assertEquals("retry this message", retryable.draft)
            assertNull(retryable.pendingId)
        }
    }

    @Test
    fun `terminal composed send requires a connected stable ready screen and no pending delivery`() {
        val ready = TerminalScreen(
            type = "screen",
            seq = 7,
            rows = 24,
            cols = 80,
            cursorRow = 23,
            cursorCol = 2,
            lines = listOf("› "),
            running = true,
            ready = true,
        )

        assertTrue(terminalSubmitEnabled("connected", ready, pendingId = null, closeRequested = false))
        assertFalse(terminalSubmitEnabled("connecting", ready, pendingId = null, closeRequested = false))
        assertFalse(
            terminalSubmitEnabled("connected", ready.copy(running = false), pendingId = null, closeRequested = false),
        )
        assertFalse(
            terminalSubmitEnabled("connected", ready.copy(ready = false), pendingId = null, closeRequested = false),
        )
        assertFalse(terminalSubmitEnabled("connected", ready, pendingId = 9L, closeRequested = false))
        assertFalse(terminalSubmitEnabled("connected", ready, pendingId = null, closeRequested = true))
    }

    @Test
    fun `terminal reconnect retries spontaneous endings without losing the draft`() {
        val current = TerminalConnectionState(
            generation = 7L,
            inputState = TerminalInputState(draft = "retry after reconnect").enqueued(5_000_000_000L),
            screenSequence = 42L,
        )

        listOf(TerminalSocketEnd.SPONTANEOUS_CLOSE, TerminalSocketEnd.FAILURE).forEach { ending ->
            val decision = terminalConnectionEnded(
                current,
                callbackGeneration = 7L,
                ending,
                screenRunning = true,
            )
            assertTrue(decision.reconnectScheduled)
            assertEquals("retry after reconnect", decision.inputState.draft)
            assertNull(decision.inputState.pendingId)
        }
    }

    @Test
    fun `terminal reconnect ignores intentional and stale socket endings`() {
        val current = TerminalConnectionState(
            generation = 7L,
            inputState = TerminalInputState(draft = "current draft").enqueued(5_000_000_000L),
            screenSequence = 42L,
        )

        listOf(TerminalSocketEnd.INTENTIONAL_CLOSE, TerminalSocketEnd.DISPOSE).forEach { ending ->
            val decision = terminalConnectionEnded(
                current,
                callbackGeneration = 7L,
                ending,
                screenRunning = true,
            )
            assertFalse(decision.reconnectScheduled)
            assertEquals("current draft", decision.inputState.draft)
            assertNull(decision.inputState.pendingId)
        }

        val staleFailure = terminalConnectionEnded(
            current,
            callbackGeneration = 6L,
            TerminalSocketEnd.FAILURE,
            screenRunning = true,
        )
        assertEquals(current, staleFailure)
    }

    @Test
    fun `terminal close after a final ended screen does not reconnect`() {
        val endedScreen = TerminalScreen(
            type = "screen",
            seq = 42L,
            rows = 24,
            cols = 80,
            cursorRow = 23,
            cursorCol = 0,
            lines = listOf("session ended"),
            running = false,
            ready = false,
        )
        val current = TerminalConnectionState(
            generation = 7L,
            inputState = TerminalInputState(draft = "preserved after completion"),
            screenSequence = 42L,
        )

        val decision = terminalConnectionEnded(
            current,
            callbackGeneration = 7L,
            TerminalSocketEnd.SPONTANEOUS_CLOSE,
            screenRunning = endedScreen.running,
        )

        assertFalse(decision.reconnectScheduled)
        assertEquals("preserved after completion", decision.inputState.draft)
    }

    @Test
    fun `terminal every fresh socket attempt resets screen generation and sequence`() {
        val visibleScreen = TerminalScreen(
            type = "screen",
            seq = 42L,
            rows = 24,
            cols = 80,
            cursorRow = 23,
            cursorCol = 0,
            lines = listOf("previous socket"),
            running = true,
            ready = true,
        )
        val initial = TerminalConnectionState(
            inputState = TerminalInputState(draft = "preserved"),
            screenSequence = 42L,
        )
        val firstAttempt = terminalSocketAttemptStarted(initial)
        assertTrue(firstAttempt.clearScreen)
        assertEquals(1L, firstAttempt.connectionState.generation)
        assertNull(firstAttempt.connectionState.screenSequence)
        assertTrue(
            terminalScreenFrameIsCurrent(firstAttempt.connectionState, callbackGeneration = 1L, sequence = 0L),
        )

        val disposed = terminalConnectionEnded(
            firstAttempt.connectionState.copy(screenSequence = visibleScreen.seq),
            callbackGeneration = 1L,
            TerminalSocketEnd.DISPOSE,
            screenRunning = visibleScreen.running,
        )
        val foregroundResume = terminalSocketAttemptStarted(disposed)

        assertTrue(foregroundResume.clearScreen)
        assertEquals(2L, foregroundResume.connectionState.generation)
        assertNull(foregroundResume.connectionState.screenSequence)
        assertFalse(foregroundResume.connectionState.reconnectScheduled)
        assertEquals("preserved", foregroundResume.connectionState.inputState.draft)
        assertTrue(
            terminalScreenFrameIsCurrent(
                foregroundResume.connectionState,
                callbackGeneration = 2L,
                sequence = 0L,
            ),
        )
        assertFalse(
            terminalScreenFrameIsCurrent(
                foregroundResume.connectionState,
                callbackGeneration = 1L,
                sequence = 43L,
            ),
        )
    }

    @Test
    fun `terminal scheduled reconnect starts a fresh socket generation`() {
        val previous = TerminalConnectionState(
            generation = 7L,
            inputState = TerminalInputState(draft = "preserved"),
            screenSequence = 42L,
            reconnectScheduled = true,
        )

        val reconnected = terminalSocketAttemptStarted(previous)

        assertTrue(reconnected.clearScreen)
        assertEquals(8L, reconnected.connectionState.generation)
        assertNull(reconnected.connectionState.screenSequence)
        assertFalse(reconnected.connectionState.reconnectScheduled)
        assertTrue(
            terminalScreenFrameIsCurrent(reconnected.connectionState, callbackGeneration = 8L, sequence = 0L),
        )
        assertFalse(
            terminalScreenFrameIsCurrent(reconnected.connectionState, callbackGeneration = 7L, sequence = 43L),
        )
    }

    @Test
    fun `terminal close command serializes an explicit close action`() {
        val close = ApiJson.json.parseToJsonElement(
            ApiJson.json.encodeToString<TerminalCommand>(TerminalCommand.Close),
        ).jsonObject

        assertEquals(JsonPrimitive("close"), close["type"])
        assertEquals(setOf("type"), close.keys)
    }

    @Test
    fun `terminal line input respects the server byte limit and preserves Unicode`() {
        val unicodeDraft = "café 漢字"

        assertEquals(
            TerminalCommand.Submit(id = 9L, text = unicodeDraft),
            terminalLineCommand(9L, unicodeDraft),
        )
        assertEquals(
            TerminalCommand.Submit(id = 10L, text = "a".repeat(4_096)),
            terminalLineCommand(10L, "a".repeat(4_096)),
        )
        assertNull(terminalLineCommand(11L, "a".repeat(4_097)))
        assertNull(terminalLineCommand(12L, "漢".repeat(1_365) + "ab"))
        assertNull(terminalLineCommand(13L, ""))
    }

    @Test
    fun `terminal rendering preserves source text and keeps cursor coordinates separate`() {
        val sourceLines = listOf("café 漢字", "second line")
        val render = terminalRenderFrame(
            TerminalScreen(
                type = "screen",
                seq = 7,
                rows = 24,
                cols = 80,
                cursorRow = 0,
                cursorCol = 6,
                lines = sourceLines,
                running = true,
                ready = true,
            ),
        )

        assertEquals(sourceLines.joinToString("\n"), render.text)
        assertEquals(0, render.cursorRow)
        assertEquals(6, render.cursorCol)
    }

    @Test
    fun `only the dedicated close action sends a terminal close command`() {
        assertNull(terminalExitCommand(TerminalExit.BACK))
        assertNull(terminalExitCommand(TerminalExit.DISPOSE))
        assertEquals(TerminalCommand.Close, terminalExitCommand(TerminalExit.CLOSE))
    }

    private fun session(
        id: String,
        status: Status,
        hidden: Boolean = false,
        companion: Boolean = false,
        title: String = id,
    ) = Session(
        backend = BackendKind.CODEX,
        id = id,
        origin = SessionOrigin.BACKGROUND,
        title = title,
        cwd = "/tmp/project",
        status = status,
        createdAtMs = 1_000,
        updatedAtMs = 2_000,
        hidden = hidden,
        companion = companion,
    )

    private fun withServer(
        responseBody: String = "",
        block: (ApiClient, List<CapturedRequest>) -> Unit,
    ) {
        LoopbackServer(responseBody).use { server ->
            block(ApiClient(server.baseUrl), server.requests)
        }
    }

    private class LoopbackServer(private val responseBody: String) : Closeable {
        private val socket = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
        val requests = CopyOnWriteArrayList<CapturedRequest>()
        val baseUrl = "http://127.0.0.1:${socket.localPort}"
        private val thread = Thread {
            try {
                while (!socket.isClosed) {
                    socket.accept().use(::serve)
                }
            } catch (error: SocketException) {
                if (!socket.isClosed) throw error
            }
        }.apply {
            name = "api-contract-loopback"
            isDaemon = true
            start()
        }

        private fun serve(connection: java.net.Socket) {
            val input = BufferedInputStream(connection.getInputStream())
            val headerText = readHeaders(input)
            val lines = headerText.split("\r\n")
            val requestLine = lines.first().split(' ')
            val headers = lines.drop(1).filter(String::isNotEmpty).associate { line ->
                val separator = line.indexOf(':')
                line.substring(0, separator).lowercase() to line.substring(separator + 1).trim()
            }
            val contentLength = headers["content-length"]?.toInt() ?: 0
            val body = ByteArray(contentLength)
            var offset = 0
            while (offset < body.size) {
                val count = input.read(body, offset, body.size - offset)
                check(count >= 0) { "request ended before its declared body length" }
                offset += count
            }
            requests += CapturedRequest(
                method = requestLine[0],
                path = requestLine[1].substringBefore('?'),
                contentType = headers["content-type"],
                body = String(body, UTF_8),
            )

            val response = responseBody.toByteArray(UTF_8)
            connection.getOutputStream().use { output ->
                output.write(
                    (
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ${response.size}\r\n" +
                            "Connection: close\r\n\r\n"
                    ).toByteArray(UTF_8),
                )
                output.write(response)
            }
        }

        private fun readHeaders(input: BufferedInputStream): String {
            val bytes = ByteArrayOutputStream()
            var tail = 0
            while (tail != 0x0d0a0d0a) {
                val byte = input.read()
                check(byte >= 0) { "request ended before its headers" }
                bytes.write(byte)
                tail = (tail shl 8 or byte) and -1
            }
            return bytes.toString(UTF_8.name())
        }

        override fun close() {
            socket.close()
            thread.join(1_000)
        }
    }

    private data class CapturedRequest(
        val method: String,
        val path: String,
        val contentType: String?,
        val body: String,
    )
}
