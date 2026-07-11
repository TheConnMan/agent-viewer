//! One-shot reply injector for a blocked session: after `send_reply` attaches (reusing the
//! same PTY + auto-Enter landing path as a normal attach), this armed state watches the
//! focused PTY and writes the reply payload once — but only after proving it is safe to do
//! so. The safety invariants (never inject into a wrong/unfocused session, never before we
//! are actually in the run, never blindly after a timeout) mirror `auto_enter`'s discipline.

use std::time::{Duration, Instant};

use crate::auto_enter::CLAUDE_AGENTS_MARKER;
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
    /// Claude: hold until auto_enter has finished AND the agents-list marker is ABSENT (we
    /// left the list into the run) before injecting — proof we are in the run, not the list,
    /// so a failed/timed-out landing never types the reply into the wrong place. Codex: false
    /// (codex resume opens straight into the pending prompt).
    pub(crate) require_run_view: bool,
    pub(crate) armed_at: Instant,
    pub(crate) ready_since: Option<Instant>,
}

/// While a reply is armed, watch its focused PTY and write the payload once it is safe:
/// only into the focused target, only after any live auto-Enter has finished and (for
/// claude) the run view is confirmed, and only after a settle. Give up after a timeout.
pub(crate) fn drive_pending_reply(ui: &mut Ui) {
    let Some(state) = ui.pending_reply.clone() else {
        return;
    };
    // Only ever inject into the currently focused reply target — never a wrong session.
    if ui.focused.as_ref() != Some(&state.key) {
        return;
    }
    // Never inject blindly late: past the timeout, drop it and let the user finish manually.
    if state.armed_at.elapsed() > REPLY_TIMEOUT {
        ui.pending_reply = None;
        ui.set_notice("reply not delivered; type it in the session".to_string());
        return;
    }
    let Some(pty) = ui.attached.get_mut(&state.key) else {
        return;
    };
    if pty.is_exited() {
        ui.pending_reply = None;
        return;
    }

    if state.require_run_view {
        // Still landing in the run (auto-Enter in flight): reset the settle and wait.
        if ui.auto_enter.is_some() {
            if let Some(p) = ui.pending_reply.as_mut() {
                p.ready_since = None;
            }
            return;
        }
        // Still on the agents list (marker present): keep waiting until the timeout so a
        // reply is never typed into the list rather than the opened run.
        let marker_present = pty.with_screen(|screen| screen.contents().contains(CLAUDE_AGENTS_MARKER));
        if marker_present {
            if let Some(p) = ui.pending_reply.as_mut() {
                p.ready_since = None;
            }
            return;
        }
    }

    // Settle, then write the payload exactly once and disarm.
    match state.ready_since {
        None => {
            if let Some(p) = ui.pending_reply.as_mut() {
                p.ready_since = Some(Instant::now());
            }
        }
        Some(since) if since.elapsed() >= REPLY_SETTLE => {
            let _ = pty.write_input(&state.payload);
            ui.pending_reply = None;
        }
        Some(_) => {}
    }
}
