package com.theconnman.agentviewer

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch

/** What a spawn came to: its folder, and the failure message if it did not start. */
internal data class SpawnOutcome(val folder: String, val error: String?)

/**
 * Runs the spawn `POST` outside the composition. A `rememberCoroutineScope` dies with the
 * activity, so a rotation mid-request dropped the success path (no dismiss, no notice, the
 * folder left folded) while the request still completed on the server, and the restored sheet
 * offered Start again: two sessions for one tap. The launcher lives as long as the process,
 * the sheet and the app observe its state, and one request is in flight at a time.
 */
internal class SpawnLauncher(
    create: suspend (NewSessionRequest) -> Unit,
    dispatcher: CoroutineDispatcher = Dispatchers.Main.immediate,
) {
    private val scope = CoroutineScope(SupervisorJob() + dispatcher)
    private val submitter = SessionSubmitter(create)

    /** The folder whose request is on the wire, or null. */
    var inFlight: String? by mutableStateOf(null)
        private set

    /** The last finished request until someone consumes it. */
    var outcome: SpawnOutcome? by mutableStateOf(null)
        private set

    /** False when a request is already in flight; nothing is sent then. */
    fun launch(request: NewSessionRequest): Boolean {
        if (inFlight != null) return false
        inFlight = request.folder
        outcome = null
        scope.launch {
            val result = submitter.submit(request)
            outcome = SpawnOutcome(
                folder = request.folder,
                error = when {
                    result == null -> "Another session is still starting"
                    result.isSuccess -> null
                    else -> result.exceptionOrNull()?.message ?: "Could not start session"
                },
            )
            inFlight = null
        }
        return true
    }

    fun consume(outcome: SpawnOutcome) {
        if (this.outcome == outcome) this.outcome = null
    }

    companion object {
        /** The process-wide launcher the app uses; tests build their own. */
        val default: SpawnLauncher by lazy { SpawnLauncher({ request -> ApiClient().create(request) }) }
    }
}

/**
 * Orders listing responses. The 2 s poll and an explicit refresh both replace `sessions`, and
 * a poll that left before a spawn can answer after the refresh that showed the new row, so a
 * response is applied only if nothing newer has been applied already.
 */
internal class ListSequencer {
    private var issued = 0L
    private var applied = 0L

    fun next(): Long = ++issued

    /** True exactly once per generation, and never for one older than the last accepted. */
    fun accept(generation: Long): Boolean {
        if (generation <= applied) return false
        applied = generation
        return true
    }
}
