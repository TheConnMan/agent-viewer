//! One-shot reply injector for a blocked session: after `send_reply` attaches (spawning a
//! fresh `claude attach` PTY, or reusing one already in the run), this armed state watches
//! the focused PTY and writes the
//! reply payload once — but only after proving it is safe to do so. The safety invariants
//! (never inject into a wrong/unfocused session, never before the attached run is actually
//! accepting input, never blindly after a timeout) gate every write.

use std::time::{Duration, Instant};

use crate::{Key, Ui};

/// The ready state must hold this long before we write the payload, so we type when the run
/// is actually accepting input rather than on a still-initializing frame.
const REPLY_SETTLE: Duration = Duration::from_millis(600);
/// Give up on a clean auto-inject after this long and tell the user to type it themselves
/// (they are already attached). Never inject blindly late.
const REPLY_TIMEOUT: Duration = Duration::from_secs(50);
/// Substrings claude prints while `claude attach` is still waking/attaching the session,
/// before its real prompt renders. While either is on screen we are not yet in the run, so
/// the injector must not type. Verified live on claude 2.1.207: the attach shows
/// "Waking session <id>…" then "Attaching…" before the session UI repaints over them.
const CLAUDE_ATTACHING_MARKERS: [&str; 2] = ["Waking session", "Attaching"];

/// One-shot reply-injection state: which PTY to write into, the bytes to write, whether the
/// run-view gate applies (claude's attach transient must clear first), whether this attach
/// spawned a fresh PTY (vs reused one already in the run), whether the attach transient has
/// been observed yet, when it was armed, and when the ready-to-write state was first observed
/// (settle debounce).
#[derive(Debug, Clone)]
pub(crate) struct PendingReply {
    pub(crate) key: Key,
    pub(crate) payload: Vec<u8>,
    /// Claude: the run-view gate applies — `claude attach` prints a "Waking…/Attaching…"
    /// transient before its prompt renders, so we must confirm that transient has cleared
    /// before injecting. Codex: false (codex resume opens straight into the pending prompt).
    pub(crate) require_run_view: bool,
    /// True when `send_reply` spawned a FRESH attach PTY for this reply (the reply feature's
    /// primary path); false when it reused a PTY already attached (already past the transient,
    /// already in the run). A fresh attach must SEE the transient appear and then clear before
    /// it is safe to type — mere marker-absence is also true on the blank pre-paint frame, so
    /// absence alone would race a slow cold-start. A reused PTY is already in the run, so
    /// absence alone is sufficient. Ignored when `require_run_view` is false (codex).
    pub(crate) fresh_attach: bool,
    /// Set once the "Waking…/Attaching…" transient has been observed on the PTY (fresh-attach
    /// gate only). Latches true and never resets, so the gate is "seen then cleared".
    pub(crate) transient_seen: bool,
    pub(crate) armed_at: Instant,
    pub(crate) ready_since: Option<Instant>,
}

/// The next step the injector should take, decided purely from observable state so the
/// safety invariants are unit-testable without a live PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyStep {
    /// Not our turn (focus elsewhere): do nothing, stay armed.
    Skip,
    /// Not ready to write yet: clear the settle timer and keep waiting.
    Wait,
    /// Ready this frame: start/continue the settle timer (do not write yet).
    Settle,
    /// Settle elapsed: write the payload and disarm.
    Write,
    /// Give up (timed out or the child exited): disarm without writing.
    Drop,
    /// The session is no longer blocked (resolved elsewhere while we attached/settled): drop
    /// without writing so a queued reply never lands in a now-ordinary session.
    Abort,
}

/// Pure gate for the injector. `focused` = the reply target is the focused PTY; `elapsed` =
/// since armed; `exited` = the PTY's child has exited; `still_blocked` = the session is still
/// NeedsInput at write time (re-checked from the live list, not just when armed);
/// `run_view_ready` = the attached run is accepting input so it is safe to type — the driver
/// precomputes this per backend/attach kind (codex: always; claude fresh attach: the
/// "Waking…/Attaching…" transient was seen and has since cleared; claude reused PTY: the
/// transient is absent); `ready_since` = elapsed since the ready state was first observed,
/// if any.
pub(crate) fn reply_step(
    focused: bool,
    elapsed: Duration,
    exited: bool,
    still_blocked: bool,
    run_view_ready: bool,
    ready_since: Option<Duration>,
) -> ReplyStep {
    if !focused {
        return ReplyStep::Skip;
    }
    if elapsed > REPLY_TIMEOUT {
        return ReplyStep::Drop;
    }
    if exited {
        return ReplyStep::Drop;
    }
    if !still_blocked {
        return ReplyStep::Abort;
    }
    if !run_view_ready {
        return ReplyStep::Wait;
    }
    match ready_since {
        None => ReplyStep::Settle,
        Some(d) if d >= REPLY_SETTLE => ReplyStep::Write,
        Some(_) => ReplyStep::Settle,
    }
}

/// While a reply is armed, watch its focused PTY and write the payload once it is safe:
/// only into the focused target, only after (for claude) the attach transient has cleared so
/// the run view is confirmed, and only after a settle. Give up after a timeout.
/// The decision is delegated to the pure [`reply_step`]; this function only gathers the
/// observable inputs and applies the resulting side effect.
pub(crate) fn drive_pending_reply(ui: &mut Ui) {
    let Some(state) = ui.pending_reply.clone() else {
        return;
    };
    // Only ever inject into the currently focused reply target — never a wrong session.
    let focused = ui.focused.as_ref() == Some(&state.key);
    let elapsed = state.armed_at.elapsed();

    // Re-check the safety gate at write time (not just when armed): the refresh worker keeps
    // `ui.app` fresh even while attached, so another client resolving the prompt shows up here
    // as the session leaving NeedsInput (or disappearing).
    let still_blocked = ui
        .app
        .session_for(&state.key)
        .is_some_and(|s| matches!(s.status, agent_viewer_core::Status::NeedsInput { .. }));

    // The PTY-observable inputs: the child's exit (always) and, only for the claude run-view
    // gate, whether the "Waking…/Attaching…" attach transient is on screen. Read both under
    // the borrow, then drop it before mutating `ui.pending_reply`. When focused but the PTY
    // entry is missing, keep the current early-return (return without disarm).
    let need_marker = state.require_run_view;
    let (exited, marker_present) = if focused {
        let Some(pty) = ui.attached.get_mut(&state.key) else {
            return;
        };
        let exited = pty.is_exited();
        let marker_present = need_marker
            && pty.with_screen(|s| {
                let contents = s.contents();
                CLAUDE_ATTACHING_MARKERS
                    .iter()
                    .any(|m| contents.contains(m))
            });
        (exited, marker_present)
    } else {
        (false, false)
    };

    // Latch "the attach transient has been seen" so a fresh attach requires an affirmative
    // seen-then-cleared transition, not mere absence. Persist the latch back into the armed
    // state; it never resets.
    if state.require_run_view
        && marker_present
        && !state.transient_seen
        && let Some(p) = ui.pending_reply.as_mut()
    {
        p.transient_seen = true;
    }
    let transient_seen = state.transient_seen || marker_present;

    // Precompute the run-view gate:
    //  - codex: always ready (resume opens straight into the pending prompt).
    //  - claude fresh attach: ready only once the "Waking…/Attaching…" transient was SEEN and
    //    then cleared. Absence alone is not enough — a freshly spawned attach PTY is blank
    //    before claude paints the transient, so injecting on absence would type into a still-
    //    initializing terminal. A very fast attach whose transient no poll pass catches leaves
    //    transient_seen=false, so the reply falls through to the 50s "type it in the session"
    //    timeout — safe (never a misdirected write) and acceptable.
    //  - claude reused PTY: already in the run from an earlier attach, so ready once the
    //    transient marker is absent (it is never expected to reappear).
    let run_view_ready = if !state.require_run_view {
        true
    } else if state.fresh_attach {
        transient_seen && !marker_present
    } else {
        !marker_present
    };

    let ready_since = state.ready_since.map(|since| since.elapsed());
    match reply_step(
        focused,
        elapsed,
        exited,
        still_blocked,
        run_view_ready,
        ready_since,
    ) {
        ReplyStep::Skip => {}
        ReplyStep::Drop => {
            // Never inject blindly late: on a timeout, tell the user to finish manually
            // (they are already attached). An exited-child drop is silent.
            if elapsed > REPLY_TIMEOUT {
                ui.set_notice("reply not delivered; type it in the session".to_string());
            }
            ui.pending_reply = None;
        }
        ReplyStep::Abort => {
            // The prompt was resolved (or the session vanished) while we attached/settled:
            // drop without writing so the reply never lands in a now-ordinary session.
            ui.pending_reply = None;
            ui.set_notice("session no longer waiting; reply not sent".to_string());
        }
        ReplyStep::Wait => {
            if let Some(p) = ui.pending_reply.as_mut() {
                p.ready_since = None;
            }
        }
        ReplyStep::Settle => {
            if let Some(p) = ui.pending_reply.as_mut()
                && p.ready_since.is_none()
            {
                p.ready_since = Some(Instant::now());
            }
        }
        ReplyStep::Write => {
            if let Some(pty) = ui.attached.get_mut(&state.key) {
                let _ = pty.write_input(&state.payload);
            }
            ui.pending_reply = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Small, PTY-free durations proving the safety invariants of the pure gate. The gate
    // takes a single precomputed `run_view_ready`; the per-backend/attach-kind distinction
    // (claude fresh attach waits for the "Waking…/Attaching…" transient to be seen and clear,
    // claude reused PTY waits for it to be absent, codex is always ready) is resolved into that
    // bool by `drive_pending_reply`.
    const SHORT: Duration = Duration::from_millis(10);

    #[test]
    fn not_focused_skips() {
        assert_eq!(
            reply_step(false, SHORT, false, true, false, None),
            ReplyStep::Skip
        );
    }

    #[test]
    fn past_timeout_drops() {
        assert_eq!(
            reply_step(true, REPLY_TIMEOUT + SHORT, false, true, false, None),
            ReplyStep::Drop
        );
    }

    #[test]
    fn exited_child_drops() {
        assert_eq!(
            reply_step(true, SHORT, true, true, false, None),
            ReplyStep::Drop
        );
    }

    #[test]
    fn no_longer_blocked_aborts() {
        // The prompt was resolved elsewhere while we attached/settled: never write.
        assert_eq!(
            reply_step(true, SHORT, false, false, false, None),
            ReplyStep::Abort
        );
    }

    #[test]
    fn not_run_view_ready_waits() {
        // The run-view gate has not resolved yet (claude's attach transient is still on
        // screen): keep waiting.
        assert_eq!(
            reply_step(true, SHORT, false, true, false, None),
            ReplyStep::Wait
        );
    }

    #[test]
    fn run_view_ready_settles_then_writes() {
        // Gate passed: ready_since drives the settle-then-write progression.
        assert_eq!(
            reply_step(true, SHORT, false, true, true, None),
            ReplyStep::Settle
        );
        assert_eq!(
            reply_step(true, SHORT, false, true, true, Some(REPLY_SETTLE + SHORT)),
            ReplyStep::Write
        );
    }

    #[test]
    fn ready_since_none_settles() {
        assert_eq!(
            reply_step(true, SHORT, false, true, true, None),
            ReplyStep::Settle
        );
    }

    #[test]
    fn ready_since_before_settle_keeps_settling() {
        assert_eq!(
            reply_step(true, SHORT, false, true, true, Some(REPLY_SETTLE - SHORT)),
            ReplyStep::Settle
        );
    }

    #[test]
    fn ready_since_past_settle_writes() {
        assert_eq!(
            reply_step(true, SHORT, false, true, true, Some(REPLY_SETTLE + SHORT)),
            ReplyStep::Write
        );
    }
}
