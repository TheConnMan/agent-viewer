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

    private fun session(
        id: String,
        status: Status,
        hidden: Boolean = false,
        companion: Boolean = false,
    ) = Session(
        backend = BackendKind.CODEX,
        id = id,
        origin = SessionOrigin.BACKGROUND,
        title = id,
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
