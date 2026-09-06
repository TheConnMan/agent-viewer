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
    fun `decodes a Grok session and lists all three backends`() {
        val session = ApiJson.json.decodeFromString<Session>(grokSessionJson)

        assertEquals(BackendKind.GROK, session.backend)
        assertEquals("thread-grok", session.id)
        assertEquals(
            listOf(BackendKind.CODEX, BackendKind.CLAUDE, BackendKind.GROK),
            BackendKind.entries.toList(),
        )
    }

    @Test
    fun `decodes a mixed Codex Claude and Grok listing without throwing`() {
        val listing = ApiJson.json.decodeFromString<List<Session>>(
            "[" + listOf(
                sessionJson("thread-codex", "Codex"),
                sessionJson("thread-claude", "Claude"),
                grokSessionJson,
            ).joinToString(",") + "]",
        )

        assertEquals(
            listOf(BackendKind.CODEX, BackendKind.CLAUDE, BackendKind.GROK),
            listing.map(Session::backend),
        )
        assertEquals(
            listOf("thread-codex", "thread-claude", "thread-grok"),
            listing.map(Session::id),
        )
    }

    @Test
    fun `terminal websocket URL names the Grok backend`() {
        assertEquals(
            "wss://remote-dev.tail26e2ce.ts.net:8443/sessions/thread-grok/terminal?backend=Grok",
            terminalWebSocketUrl(ApiConfig.DEFAULT_BASE_URL, "thread-grok", BackendKind.GROK),
        )
    }

    @Test
    fun `decodes a server truncated title and summary without losing the ellipsis`() {
        val title = "t".repeat(239) + "\u2026"
        val summary = "s".repeat(239) + "\u2026"
        val session = ApiJson.json.decodeFromString<Session>(
            """
            {
              "backend": "Claude",
              "id": "thread-truncated",
              "origin": "Interactive",
              "title": "$title",
              "cwd": "/home/brian/git/agent-viewer",
              "status": "Working",
              "created_at_ms": 1724150000000,
              "updated_at_ms": 1724151000000,
              "summary": "$summary"
            }
            """.trimIndent(),
        )

        assertEquals(240, session.title.length)
        assertEquals(240, session.summary.length)
        assertTrue(session.title.endsWith("\u2026"))
        assertTrue(session.summary.endsWith("\u2026"))
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
            model = null,
            folder = "/home/brian/git/agent-viewer",
            task = "Start the companion server",
        )
        val encoded = ApiJson.json.encodeToString(request)

        assertTrue(encoded.contains("\"provider\":\"auto\""))
        // The phone has no model picker: the field is sent as JSON null, which the server's
        // `Option<String>` reads as "route it", never as a model literally named "null".
        assertTrue(encoded.contains("\"model\":null"), encoded)
        assertTrue(encoded.contains("\"dir\":\"/home/brian/git/agent-viewer\""))
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
    fun `the filter row offers all, by project and by state with no hidden chip`() {
        assertEquals(
            listOf("all", "by project", "by state"),
            sessionFilters.map { (_, label) -> label },
        )
        assertEquals(
            listOf(SessionFilter.ALL, SessionFilter.PROJECT, SessionFilter.STATE),
            SessionFilter.entries.toList(),
        )
    }

    @Test
    fun `sessions requests the bare sessions path with no query parameters`() = withServer(
        responseBody = "[]",
    ) { client, requests ->
        assertEquals(emptyList<Session>(), runBlocking { client.sessions() })

        assertEquals(
            listOf(CapturedRequest("GET", "/sessions", null, "")),
            requests,
        )
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
            listOf("visible-waiting"),
            triageSessionRows(sessions).map(Session::id),
        )
    }

    @Test
    fun `triage queues needs input first then done and idle, longest wait first in each group`() {
        // Mirrors the TUI's `triage_queue`: every needs-input session, then every shown idle or
        // done session, ascending updated_at within each group. Working, error and unknown rows
        // are not reviewable and stay out; hidden, companion and hold rows never reach the phone
        // list in the first place.
        val sessions = listOf(
            session("done-old", Status.Done, updatedAtMs = 1_000),
            session("working", Status.Working, updatedAtMs = 500),
            session("waiting-new", Status.NeedsInput("Approve it"), updatedAtMs = 9_000),
            session("idle-new", Status.Idle, updatedAtMs = 8_000),
            session("error", Status.Error, updatedAtMs = 100),
            session("waiting-old", Status.NeedsInput(null), updatedAtMs = 3_000),
            session("unknown", Status.Unknown, updatedAtMs = 200),
            session("done-hidden", Status.Done, hidden = true, updatedAtMs = 50),
            session("done-hold", Status.Done, title = "Hold", updatedAtMs = 60),
        )

        assertEquals(
            listOf("waiting-old", "waiting-new", "done-old", "idle-new"),
            triageSessionRows(sessions).map(Session::id),
        )
    }

    @Test
    fun `the capability cache key changes with the state that decides the capabilities`() {
        // Claude advertises stop only while unfinished and the archive kind follows hidden, so a
        // cache keyed by identity alone would keep serving a stale answer after a status change.
        val working = session("s", Status.Working)
        val done = session("s", Status.Done)
        val hidden = session("s", Status.Done, hidden = true)
        val later = session("s", Status.Working, updatedAtMs = 9_000)

        assertEquals(working.key(), done.key())
        assertTrue(working.capabilityCacheKey() != done.capabilityCacheKey())
        assertTrue(done.capabilityCacheKey() != hidden.capabilityCacheKey())
        assertEquals(working.capabilityCacheKey(), later.capabilityCacheKey(), "a timestamp alone does not invalidate")
        assertTrue(
            working.capabilityCacheKey().startsWith("${BackendKind.CODEX.name}:s:"),
            "the key is scoped to the backend and id",
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
            listOf("triageLongerTitle"),
            triageSessionRows(sessions).map(Session::id),
        )
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

    private val grokSessionJson = sessionJson("thread-grok", "Grok")

    private fun sessionJson(id: String, backend: String) = """
        {
          "backend": "$backend",
          "id": "$id",
          "origin": "Background",
          "title": "Drain the bonus backlog",
          "cwd": "/home/brian/git/agent-viewer",
          "status": "Working",
          "created_at_ms": 1724150000000,
          "updated_at_ms": 1724151000000
        }
    """.trimIndent()

    private fun session(
        id: String,
        status: Status,
        hidden: Boolean = false,
        companion: Boolean = false,
        title: String = id,
        updatedAtMs: Long = 2_000,
    ) = Session(
        backend = BackendKind.CODEX,
        id = id,
        origin = SessionOrigin.BACKGROUND,
        title = title,
        cwd = "/tmp/project",
        status = status,
        createdAtMs = 1_000,
        updatedAtMs = updatedAtMs,
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
}
