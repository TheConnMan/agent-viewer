package com.theconnman.agentviewer

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue
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
}
