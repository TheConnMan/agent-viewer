package com.theconnman.agentviewer

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNull
import kotlin.test.assertTrue
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers

/** The launcher's promises: one request at a time, an outcome per request, nothing lost. */
class SpawnLauncherTest {
    private val request = NewSessionRequest(ComposerProvider.AUTO, null, "/tmp/project", "Start it")

    @Test
    fun `one request at a time, and the outcome names the folder`() {
        val gate = CompletableDeferred<Unit>()
        var creates = 0
        val launcher = SpawnLauncher({
            creates += 1
            gate.await()
        }, Dispatchers.Unconfined)

        assertTrue(launcher.launch(request))
        assertEquals("/tmp/project", launcher.inFlight)
        assertFalse(launcher.launch(request.copy(folder = "/tmp/other")), "a second tap while starting sends nothing")
        assertEquals(1, creates)

        gate.complete(Unit)
        assertNull(launcher.inFlight)
        assertEquals(SpawnOutcome("/tmp/project", error = null), launcher.outcome)

        launcher.consume(launcher.outcome!!)
        assertNull(launcher.outcome)
        assertTrue(launcher.launch(request), "free again once the outcome is consumed or not")
    }

    @Test
    fun `a failure carries the server's message and frees the launcher`() {
        val launcher = SpawnLauncher({ throw java.io.IOException("POST /sessions failed with HTTP 501: auto provider is unavailable") }, Dispatchers.Unconfined)
        assertTrue(launcher.launch(request))
        assertNull(launcher.inFlight)
        assertEquals("POST /sessions failed with HTTP 501: auto provider is unavailable", launcher.outcome?.error)
        assertTrue(launcher.launch(request), "a failed request does not block the next")
    }

    @Test
    fun `consume ignores an outcome that is no longer the current one`() {
        val launcher = SpawnLauncher({}, Dispatchers.Unconfined)
        launcher.launch(request)
        val first = launcher.outcome!!
        launcher.launch(request.copy(folder = "/tmp/other"))
        launcher.consume(first)
        assertEquals("/tmp/other", launcher.outcome?.folder)
    }

    @Test
    fun `listing responses apply in order and an older one never overwrites a newer`() {
        val sequence = ListSequencer()
        val poll = sequence.next()
        val refresh = sequence.next()
        assertTrue(sequence.accept(refresh), "the refresh that showed the new row")
        assertFalse(sequence.accept(poll), "the poll that left before the spawn")
        assertFalse(sequence.accept(refresh), "and nothing applies twice")
        assertTrue(sequence.accept(sequence.next()))
    }
}
