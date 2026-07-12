//! Blocking backend mutations (stop/remove/rename/hide), each run to completion on a
//! `MutationRunner` worker thread with all data owned (Send).

use agent_viewer_core::backend::{Backend, BackendKind};
use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::opencode::OpencodeBackend;
use agent_viewer_core::state::ViewerDb;
use agent_viewer_core::{Session, default_codex_home};

/// A blocking backend mutation, run on a worker thread with all data owned (Send).
pub(crate) enum Mutation {
    Stop(Session),
    Remove(Session),
    Rename(Session, String),
    Hide(Session),
    Unhide(Session),
}

/// A fresh backend instance for a worker thread. The mutating methods (stop/remove/
/// rename/hide) depend only on the passed id/session, never on cached list state, so a
/// fresh instance behaves identically to the one in the main `backends` slice.
fn fresh_backend(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Codex => Box::new(CodexBackend::new(default_codex_home())),
        BackendKind::Claude => Box::new(ClaudeBackend::new()),
        BackendKind::Opencode => Box::new(OpencodeBackend::new()),
    }
}

/// Run one mutation to completion, applying its viewer-DB follow-up against a fresh
/// connection so the render loop never blocks. Returns the user-facing result message.
pub(crate) fn run_mutation(m: Mutation) -> Result<String, String> {
    match m {
        Mutation::Stop(s) => match fresh_backend(s.backend).stop(&s) {
            Ok(()) => {
                if let Ok(db) = ViewerDb::open_default() {
                    let _ = db.mark_stopped(s.backend, &s.id);
                }
                Ok(format!("stopped — {}", s.title))
            }
            Err(e) => Err(format!("stop failed: {e}")),
        },
        Mutation::Remove(s) => {
            // Terminate the live process FIRST, inside this same thread, before archiving or
            // deleting. Two-stage Ctrl+X submits `stop` then `remove` on different dedup keys,
            // so the two race; killing here guarantees ordering within the remove op and makes
            // a concurrent `stop` harmless — terminate is idempotent (ESRCH/gone -> Ok) and
            // pid-guarded by comm prefix, so it never signals a recycled pid.
            if let Some(pid) = s.pid {
                let _ = agent_viewer_core::spawn::terminate(pid, s.backend.name());
            }
            fresh_backend(s.backend)
                .remove(&s.id)
                .map(|()| format!("removed — {}", s.title))
                .map_err(|e| format!("remove failed: {e}"))
        }
        Mutation::Rename(s, name) => match fresh_backend(s.backend).rename(&s, &name) {
            Ok(()) => {
                // A prior daemon-down rename may have left a stale override; clear it so
                // the native title shows through.
                if let Ok(db) = ViewerDb::open_default() {
                    let _ = db.clear_name_override(s.backend, &s.id);
                }
                Ok(format!("renamed {}", s.backend.name()))
            }
            Err(e) => {
                // claude has no working external rename channel (the Fleet View rendezvous
                // socket rejects rename frames), so every claude rename falls back to the
                // viewer-local name override, which the state overlay applies to the row.
                if s.backend == BackendKind::Claude
                    && let Ok(db) = ViewerDb::open_default()
                    && db.set_name_override(s.backend, &s.id, &name).is_ok()
                {
                    return Ok("renamed (local override)".to_string());
                }
                Err(format!("rename failed: {e}"))
            }
        },
        Mutation::Hide(s) => fresh_backend(s.backend)
            .hide(&s.id)
            .map(|()| format!("archived — {}", s.title))
            .map_err(|e| format!("{}: {e}", s.backend.name())),
        Mutation::Unhide(s) => fresh_backend(s.backend)
            .unhide(&s.id)
            .map(|()| format!("unarchived — {}", s.title))
            .map_err(|e| format!("{}: {e}", s.backend.name())),
    }
}
