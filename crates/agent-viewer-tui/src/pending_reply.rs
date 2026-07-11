//! One-shot reply injector for a blocked session: after `send_reply` attaches (reusing the
//! same PTY + auto-Enter landing path as a normal attach), this armed state watches the
//! focused PTY and writes the reply payload once — but only after proving it is safe to do
//! so. The safety invariants (never inject into a wrong/unfocused session, never before we
//! are actually in the run, never blindly after a timeout) mirror `auto_enter`'s discipline.

use std::time::{Duration, Instant};

use crate::{Key, Ui};

/// The marker/landing state must hold this long before we write the payload, so we type when
/// the run is actually accepting input rather than on a still-initializing frame.
const REPLY_SETTLE: Duration = Duration::from_millis(600);
/// Give up on a clean auto-inject after this long and tell the user to type it themselves
/// (they are already attached). Never inject blindly late.
const REPLY_TIMEOUT: Duration = Duration::from_secs(50);

/// One-shot reply-injection state: which PTY to write into, the bytes to write, whether we
/// must confirm we have left the claude agents list into the run first, when it was armed,
/// and when the ready-to-write state was first observed (settle debounce).
#[derive(Debug, Clone)]
pub(crate) struct PendingReply {
    pub(crate) key: Key,
    pub(crate) payload: Vec<u8>,
    /// Claude: hold until auto_enter reports an explicit SUCCESSFUL landing for this key (its
    /// Enter opened the run) before injecting — proof we are in the run, not the list, so a
    /// failed/timed-out landing (which clears auto_enter WITHOUT setting the landed signal)
    /// never types the reply into the wrong place. Codex: false (codex resume opens straight
    /// into the pending prompt).
    pub(crate) require_run_view: bool,
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
/// `require_run_view` = the claude run-view gate applies; `landed` = auto_enter reported an
/// explicit successful landing in the run for this key; `ready_since` = elapsed since the
/// ready state was first observed, if any.
pub(crate) fn reply_step(
    focused: bool,
    elapsed: Duration,
    exited: bool,
    still_blocked: bool,
    require_run_view: bool,
    landed: bool,
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
    if require_run_view && !landed {
        return ReplyStep::Wait;
    }
    match ready_since {
        None => ReplyStep::Settle,
        Some(d) if d >= REPLY_SETTLE => ReplyStep::Write,
        Some(_) => ReplyStep::Settle,
    }
}

/// While a reply is armed, watch its focused PTY and write the payload once it is safe:
/// only into the focused target, only after any live auto-Enter has finished and (for
/// claude) the run view is confirmed, and only after a settle. Give up after a timeout.
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
    // as the session leaving NeedsInput (or disappearing). Landing is proven by the explicit
    // success signal auto_enter records, never by a timed-out clear.
    let still_blocked = ui
        .app
        .session_for(&state.key)
        .is_some_and(|s| matches!(s.status, agent_viewer_core::Status::NeedsInput));
    let landed = ui.auto_enter_landed.as_ref() == Some(&state.key);

    // The only PTY-observable input we still need is the child's exit; read it under the
    // borrow, then drop it before mutating `ui.pending_reply`. When focused but the PTY entry
    // is missing, keep the current early-return (return without disarm).
    let exited = if focused {
        let Some(pty) = ui.attached.get_mut(&state.key) else {
            return;
        };
        pty.is_exited()
    } else {
        false
    };

    let ready_since = state.ready_since.map(|since| since.elapsed());
    match reply_step(
        focused,
        elapsed,
        exited,
        still_blocked,
        state.require_run_view,
        landed,
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

    // Small, PTY-free durations proving the safety invariants of the pure gate.
    const SHORT: Duration = Duration::from_millis(10);

    #[test]
    fn not_focused_skips() {
        assert_eq!(
            reply_step(false, SHORT, false, true, false, false, None),
            ReplyStep::Skip
        );
    }

    #[test]
    fn past_timeout_drops() {
        assert_eq!(
            reply_step(true, REPLY_TIMEOUT + SHORT, false, true, false, false, None),
            ReplyStep::Drop
        );
    }

    #[test]
    fn exited_child_drops() {
        assert_eq!(
            reply_step(true, SHORT, true, true, false, false, None),
            ReplyStep::Drop
        );
    }

    #[test]
    fn no_longer_blocked_aborts() {
        // The prompt was resolved elsewhere while we attached/settled: never write.
        assert_eq!(
            reply_step(true, SHORT, false, false, false, false, None),
            ReplyStep::Abort
        );
    }

    #[test]
    fn run_view_waits_until_landed() {
        // require_run_view = true but auto_enter has not reported a successful landing yet.
        assert_eq!(
            reply_step(true, SHORT, false, true, true, false, None),
            ReplyStep::Wait
        );
    }

    #[test]
    fn run_view_gate_skipped_when_not_required() {
        // require_run_view = false: landing never blocks, so we settle regardless.
        assert_eq!(
            reply_step(true, SHORT, false, true, false, false, None),
            ReplyStep::Settle
        );
    }

    #[test]
    fn run_view_landed_settles_then_writes() {
        // require_run_view = true AND landed: the gate is passed, so ready_since drives the
        // settle-then-write progression.
        assert_eq!(
            reply_step(true, SHORT, false, true, true, true, None),
            ReplyStep::Settle
        );
        assert_eq!(
            reply_step(true, SHORT, false, true, true, true, Some(REPLY_SETTLE + SHORT)),
            ReplyStep::Write
        );
    }

    #[test]
    fn ready_since_none_settles() {
        assert_eq!(
            reply_step(true, SHORT, false, true, false, false, None),
            ReplyStep::Settle
        );
    }

    #[test]
    fn ready_since_before_settle_keeps_settling() {
        assert_eq!(
            reply_step(true, SHORT, false, true, false, false, Some(REPLY_SETTLE - SHORT)),
            ReplyStep::Settle
        );
    }

    #[test]
    fn ready_since_past_settle_writes() {
        assert_eq!(
            reply_step(true, SHORT, false, true, false, false, Some(REPLY_SETTLE + SHORT)),
            ReplyStep::Write
        );
    }
}
