//! One-shot auto-Enter driver for a live claude attach: watches the PTY for the agents
//! view and presses Enter (after a settle) so the attach lands IN the preselected run.
//! Moved verbatim from `main.rs`; the logic has a documented review disposition — do not
//! alter it here.

use std::time::{Duration, Instant};

use crate::{Key, Ui};

/// How long to keep watching a live claude attach for the agents view before giving up on
/// the one-shot auto-Enter. Generous because `claude agents` can take 8-15s+ to boot
/// (plugin/MCP startup); harmless while armed since any user key disarms it.
const AUTO_ENTER_TIMEOUT: Duration = Duration::from_secs(45);
/// The marker must stay visible this long before we press Enter, so we land when claude is
/// actually accepting input rather than on the first painted (still-initializing) frame.
const AUTO_ENTER_SETTLE: Duration = Duration::from_millis(500);
/// Stage-1 marker: the agents list is up and the preselected row is ready. Also used by the
/// pending-reply injector to prove we have left the list into the run before typing a reply.
pub(crate) const CLAUDE_AGENTS_MARKER: &str = "describe a task for a new session";
/// Stage-2 markers (fallback only): if the first Enter merely expanded a collapsed row
/// rather than opening the run, either collapse-hint variant shows and a second Enter opens.
const CLAUDE_EXPANDED_MARKER: &str = "enter to collapse";
const CLAUDE_EXPANDED_MARKER_ALT: &str = "space to reply";

/// Two-stage auto-Enter: `CLAUDE_AGENTS_SELECT` preselects the row but does not expand it,
/// so opening the run takes two returns — one to expand, one to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoEnterStage {
    /// Waiting for the agents list; a settled Enter expands the preselected row.
    AwaitingList,
    /// Row expanded; a settled Enter on the collapse hint opens the run, then disarms.
    AwaitingExpanded,
}

/// One-shot auto-Enter state for a live claude attach: which PTY, when it was armed, which
/// stage we are on, when the current stage's marker was first seen (settle debounce), and
/// (stage 2 only) whether the expanded-row hint was ever seen ABSENT since stage 1's Enter —
/// proof of a genuine collapsed->expanded transition, so a pre-existing hint can't queue a
/// stray Enter into the just-opened run.
#[derive(Debug, Clone)]
pub(crate) struct AutoEnter {
    pub(crate) key: Key,
    pub(crate) armed_at: Instant,
    pub(crate) stage: AutoEnterStage,
    pub(crate) marker_since: Option<Instant>,
    pub(crate) expanded_absent_seen: bool,
}

/// While a live claude attach is armed, watch its PTY for the agents view and press Enter
/// once so we land IN the preselected run rather than sitting on the agents list (the
/// internal autoOpenJobId is not reachable via env/flag). Give up after a timeout; the
/// arming is cleared on any user key or when the PTY is pruned.
pub(crate) fn drive_auto_enter(ui: &mut Ui) {
    let Some(state) = ui.auto_enter.clone() else {
        return;
    };
    // Only drive the currently focused attach.
    if ui.focused.as_ref() != Some(&state.key) {
        return;
    }
    if state.armed_at.elapsed() > AUTO_ENTER_TIMEOUT {
        ui.auto_enter = None;
        return;
    }
    let Some(pty) = ui.attached.get_mut(&state.key) else {
        return;
    };
    // Snapshot the two markers we care about this frame under one screen lock.
    let (list_present, expanded_present) = pty.with_screen(|screen| {
        let c = screen.contents();
        (
            c.contains(CLAUDE_AGENTS_MARKER),
            c.contains(CLAUDE_EXPANDED_MARKER) || c.contains(CLAUDE_EXPANDED_MARKER_ALT),
        )
    });

    match state.stage {
        AutoEnterStage::AwaitingList => {
            if !list_present {
                if let Some(ae) = &mut ui.auto_enter {
                    ae.marker_since = None;
                }
                return;
            }
            match state.marker_since {
                None => {
                    if let Some(ae) = &mut ui.auto_enter {
                        ae.marker_since = Some(Instant::now());
                    }
                }
                Some(since) if since.elapsed() >= AUTO_ENTER_SETTLE => {
                    // With real preselection (CLAUDE_AGENTS_SELECT now reaches the child) the
                    // row comes up pre-expanded, so this Enter opens the run directly. Advance
                    // to stage 2 only as a fallback for a collapsed variant. Seed the
                    // "hint seen absent" flag from THIS pre-Enter frame — the row is not yet
                    // expanded here, so its hint is absent — otherwise a hint that paints within
                    // one poll pass would leave the flag unset and stage 2 would never fire.
                    let _ = pty.write_input(b"\r");
                    if let Some(ae) = &mut ui.auto_enter {
                        ae.stage = AutoEnterStage::AwaitingExpanded;
                        ae.marker_since = None;
                        ae.expanded_absent_seen = stage2_seed_absent(expanded_present);
                    }
                }
                Some(_) => {}
            }
        }
        AutoEnterStage::AwaitingExpanded => {
            // Stage-1's Enter opened the run: the list marker is gone -> we've left the list,
            // so disarm without pressing (never queue a stray Enter into the opened run).
            if !list_present {
                ui.auto_enter_landed = Some(state.key.clone());
                ui.auto_enter = None;
                return;
            }
            // Still on the list. The fallback second Enter may fire only after a GENUINE
            // collapsed->expanded transition — i.e. the hint was observed ABSENT at least once
            // since stage 1 — so a hint that was already on screen can't trigger a stray press.
            if !expanded_present {
                if let Some(ae) = &mut ui.auto_enter {
                    ae.expanded_absent_seen = true;
                    ae.marker_since = None;
                }
                return;
            }
            if !stage2_ready(expanded_present, state.expanded_absent_seen) {
                return;
            }
            match state.marker_since {
                None => {
                    if let Some(ae) = &mut ui.auto_enter {
                        ae.marker_since = Some(Instant::now());
                    }
                }
                Some(since) if since.elapsed() >= AUTO_ENTER_SETTLE => {
                    let _ = pty.write_input(b"\r");
                    ui.auto_enter_landed = Some(state.key.clone());
                    ui.auto_enter = None;
                }
                Some(_) => {}
            }
        }
    }
}

/// Initial `expanded_absent_seen` at the stage-1 -> stage-2 transition: seeded from the
/// pre-Enter frame, where the row is not yet expanded so its hint is absent. Only a hint
/// already on screen at the transition (should not happen) leaves it unseeded.
fn stage2_seed_absent(expanded_present_at_transition: bool) -> bool {
    !expanded_present_at_transition
}

/// Whether stage 2's fallback Enter may fire this frame: the expanded hint is on screen AND
/// it was observed absent at least once (a genuine collapsed->expanded transition, not a
/// hint that was already up). The settle timing is enforced separately.
fn stage2_ready(expanded_present: bool, expanded_absent_seen: bool) -> bool {
    expanded_present && expanded_absent_seen
}

#[cfg(test)]
mod tests {
    use super::{stage2_ready, stage2_seed_absent};

    #[test]
    fn stage2_seeds_absent_from_pre_enter_frame_so_fast_paint_still_fires() {
        // At the stage-1 -> stage-2 transition the row is not yet expanded, so its hint is
        // absent — that observation is seeded, not discarded.
        let seen = stage2_seed_absent(/* expanded_present_at_transition = */ false);
        assert!(seen);
        // Fast paint: the hint appears on the very next poll pass. Stage 2 is ready without
        // needing to sample a separate absent frame first (the bug: it used to never fire).
        assert!(stage2_ready(/* expanded_present = */ true, seen));

        // A hint somehow already up AT the transition leaves the flag unseeded, so stage 2
        // waits for a genuine absent->present transition rather than firing a stray Enter.
        let unseen = stage2_seed_absent(true);
        assert!(!unseen);
        assert!(!stage2_ready(true, unseen));
        // And with the hint not on screen this frame, stage 2 is never ready regardless.
        assert!(!stage2_ready(false, true));
    }
}
