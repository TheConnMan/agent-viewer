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
            let backend = fresh_backend(s.backend);
            // Refuse BEFORE the terminate below. `remove` is advertised per backend but gated
            // per row (claude needs a short id), so an id-less row used to be SIGTERMed and
            // only then declined — killing a live session that stayed in the list.
            if !backend.can_remove(&s) {
                return Err(format!("{} does not support remove", s.backend.name()));
            }
            // Terminate the live process FIRST, inside this same thread, before archiving or
            // deleting. Two-stage Ctrl+X submits `stop` then `remove` on different dedup keys,
            // so the two race; killing here guarantees ordering within the remove op and makes
            // a concurrent `stop` harmless — terminate is idempotent (ESRCH/gone -> Ok) and
            // pid-guarded by comm prefix, so it never signals a recycled pid.
            if let Some(pid) = s.pid {
                let _ = agent_viewer_core::spawn::terminate(pid, s.backend.name());
            }
            match backend.remove(&s) {
                Ok(()) => Ok(format!("removed — {}", s.title)),
                // A row with no bg job to remove (id-less) is a capability miss, not a
                // failure: surface it as the invariant's benign "not supported" notice, the
                // same shape as the stop/rename unsupported messaging. Genuine CLI failures
                // (Error::Command) stay a "remove failed" error.
                Err(agent_viewer_core::error::Error::Unsupported(name)) => {
                    Err(format!("{name} does not support remove"))
                }
                Err(e) => Err(format!("remove failed: {e}")),
            }
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
            Err(e) => Err(format!("rename failed: {e}")),
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

#[cfg(test)]
mod tests {
    use super::{Mutation, run_mutation};
    use agent_viewer_core::backend::{BackendKind, Status};
    use agent_viewer_core::Session;
    use std::time::Duration;

    /// A live process whose `/proc/<pid>/comm` starts with "claude", which is the only shape
    /// `spawn::terminate`'s pid-reuse guard will actually signal. Built by copying a sleeper
    /// under a claude-prefixed name; a plain `sleep` would be spared by the guard and so
    /// could not detect the defect at all.
    fn claude_named_victim(tag: &str) -> (std::path::PathBuf, std::process::Child) {
        let dir = std::env::temp_dir().join(format!("av-ops-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let bin = dir.join("claude-remove-victim");
        std::fs::copy("/bin/sleep", &bin).expect("copy sleeper");
        let child = std::process::Command::new(&bin)
            .arg("30")
            .spawn()
            .expect("spawn victim");
        (dir, child)
    }

    fn claude_session(short_id: Option<&str>, pid: u32) -> Session {
        Session {
            backend: BackendKind::Claude,
            id: "3f9c1a2e-0000-4000-8000-000000000001".to_string(),
            short_id: short_id.map(str::to_string),
            title: "probe".to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            created_at_ms: 0,
            updated_at_ms: 0,
            status: Status::Working,
            hidden: false,
            source_label: String::new(),
            summary: String::new(),
            companion: false,
            pid: Some(pid),
            rollout_path: None,
            pr_refs: Vec::new(),
        }
    }

    // The defect: `remove` is advertised backend-wide for claude but gated per row on the
    // short id, so an interactive row passed the capability gate, got its process group
    // SIGTERMed, and only then was declined. The session died and stayed in the list.
    #[test]
    fn unsupported_remove_never_terminates_the_live_process() {
        let (dir, mut victim) = claude_named_victim("unsupported");
        let session = claude_session(None, victim.id());

        let result = run_mutation(Mutation::Remove(session));

        // Give a stray SIGTERM time to land before asserting the process survived.
        std::thread::sleep(Duration::from_millis(250));
        let alive = victim.try_wait().expect("try_wait").is_none();

        let _ = victim.kill();
        let _ = victim.wait();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            result,
            Err("claude does not support remove".to_string()),
            "an id-less claude row must be declined"
        );
        assert!(
            alive,
            "unsupported remove killed the live process before declining"
        );
    }
}
