//! Blocking backend mutations (stop/remove/rename/hide), each run to completion on a
//! `MutationRunner` worker thread with all data owned (Send).

use std::collections::HashSet;

use agent_viewer_core::backend::{Backend, BackendKind};
use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::opencode::{OpencodeBackend, OpencodeRuntime};
use agent_viewer_core::{Session, SpawnResult, default_codex_home};
use agent_viewer_tui::app::App;
use agent_viewer_tui::mutations::{MutationOutcome, SpawnSelection};

/// A blocking backend mutation, run on a worker thread with all data owned (Send).
pub(crate) enum Mutation {
    Stop(Session),
    Remove(Session),
    Rename(Session, String),
    Hide(Session),
    Unhide(Session),
    /// Spawn a new session. On the runner, not the key path: a codex spawn now talks to the
    /// app-server daemon and may start one, so it is a multi-second blocking call on a bad day
    /// and would freeze the composer if it ran inline like it used to.
    Spawn {
        backend: BackendKind,
        dir: std::path::PathBuf,
        task: String,
        model: Option<String>,
        spawned_at_ms: i64,
        preexisting_ids: HashSet<String>,
        notice: String,
    },
}

impl Mutation {
    pub(crate) fn spawn(
        app: &App,
        backend: BackendKind,
        dir: std::path::PathBuf,
        task: String,
        model: Option<String>,
        spawned_at_ms: i64,
        notice: String,
    ) -> Self {
        Self::Spawn {
            backend,
            dir,
            task,
            model,
            spawned_at_ms,
            preexisting_ids: app.session_ids_for_backend(backend),
            notice,
        }
    }
}

fn spawn_selection_from_mutation(
    mutation: &Mutation,
    spawned: &SpawnResult,
) -> Option<SpawnSelection> {
    let Mutation::Spawn {
        backend,
        dir,
        spawned_at_ms,
        preexisting_ids,
        ..
    } = mutation
    else {
        return None;
    };
    Some(SpawnSelection {
        backend: *backend,
        session_id: spawned.session_id.clone(),
        cwd: dir.clone(),
        spawned_at_ms: *spawned_at_ms,
        preexisting_ids: preexisting_ids.clone(),
    })
}

/// A fresh backend instance for a worker thread. The mutating methods (stop/remove/
/// rename/hide) depend only on the passed id/session, never on cached list state, so a
/// fresh instance behaves identically to the one in the main `backends` slice.
fn fresh_backend(kind: BackendKind, opencode_runtime: &OpencodeRuntime) -> Box<dyn Backend> {
    match kind {
        BackendKind::Codex => Box::new(CodexBackend::new(default_codex_home())),
        BackendKind::Claude => Box::new(ClaudeBackend::new()),
        BackendKind::Opencode => Box::new(OpencodeBackend::with_runtime(opencode_runtime.clone())),
    }
}

/// Run one mutation to completion, applying its viewer-DB follow-up against a fresh
/// connection so the render loop never blocks. Returns the user-facing notice and any
/// successful spawn identity needed by the UI.
#[cfg(test)]
pub(crate) fn run_mutation(m: Mutation) -> Result<MutationOutcome, String> {
    run_mutation_with_opencode(m, OpencodeRuntime::new())
}

/// Run one mutation with the OpenCode runtime shared by listing and actions.
pub(crate) fn run_mutation_with_opencode(
    m: Mutation,
    opencode_runtime: OpencodeRuntime,
) -> Result<MutationOutcome, String> {
    match m {
        Mutation::Stop(s) => match fresh_backend(s.backend, &opencode_runtime).stop(&s) {
            Ok(()) => Ok(MutationOutcome {
                notice: format!("stopped: {}", s.title),
                spawned: None,
            }),
            Err(e) => Err(format!("stop failed: {e}")),
        },
        Mutation::Remove(s) => {
            let backend = fresh_backend(s.backend, &opencode_runtime);
            // Refuse BEFORE the terminate below. `remove` is advertised per backend but gated
            // per row (claude needs a short id), so an idless row used to be SIGTERMed and
            // only then declined, killing a live session that stayed in the list.
            if !backend.capabilities_for(&s).delete {
                return Err(format!("{} does not support remove", s.backend.name()));
            }
            // Terminate the live process FIRST, inside this same thread, before archiving or
            // deleting. Two-stage Ctrl+X submits `stop` then `remove` on different dedup keys,
            // so the two race; killing here guarantees ordering within the remove op and makes
            // a concurrent `stop` harmless. Terminate is idempotent (ESRCH/gone -> Ok) and
            // pid guarded by comm prefix, so it never signals a recycled pid.
            //
            // NEVER for a shared runtime row. Any pid would belong to the host process and
            // every session inside it. `stop` handles these through the backend runtime, and
            // `remove` performs only the backend mutation.
            if let Some(pid) = s.pid.filter(|_| !s.daemon_hosted) {
                let _ = agent_viewer_core::spawn::terminate(pid, s.backend.name());
            }
            match backend.remove(&s) {
                Ok(()) => Ok(MutationOutcome {
                    notice: format!("removed: {}", s.title),
                    spawned: None,
                }),
                // A row with no bg job to remove (idless) is a capability miss, not a
                // failure: surface it as the invariant's benign "not supported" notice, the
                // same shape as the stop/rename unsupported messaging. Genuine CLI failures
                // (Error::Command) stay a "remove failed" error.
                Err(agent_viewer_core::error::Error::Unsupported(name)) => {
                    Err(format!("{name} does not support remove"))
                }
                Err(e) => Err(format!("remove failed: {e}")),
            }
        }
        Mutation::Rename(s, name) => {
            match fresh_backend(s.backend, &opencode_runtime).rename(&s, &name) {
                Ok(()) => Ok(MutationOutcome {
                    notice: format!("renamed {}", s.backend.name()),
                    spawned: None,
                }),
                Err(e) => Err(format!("rename failed: {e}")),
            }
        }
        Mutation::Hide(s) => fresh_backend(s.backend, &opencode_runtime)
            .hide(&s.id)
            .map(|()| MutationOutcome {
                notice: format!("archived: {}", s.title),
                spawned: None,
            })
            .map_err(|e| format!("{}: {e}", s.backend.name())),
        mutation @ Mutation::Spawn { .. } => {
            let Mutation::Spawn {
                backend,
                dir,
                task,
                model,
                spawned_at_ms,
                notice,
                ..
            } = &mutation
            else {
                unreachable!();
            };
            match fresh_backend(*backend, &opencode_runtime).spawn(dir, task, model.as_deref()) {
                // A pid means the viewer forked the worker itself, so record the spawn for the
                // pin/stop overlay against a fresh connection, like every other viewer-DB
                // follow-up here. Every successful spawn also carries enough metadata for the
                // UI to select its row when it first appears.
                Ok(spawned) => {
                    if let Some(pid) = spawned.pid
                        && let Ok(db) = agent_viewer_core::state::ViewerDb::open_default()
                    {
                        let _ = db.record_spawn(*backend, dir, pid, *spawned_at_ms);
                    }
                    Ok(MutationOutcome {
                        notice: notice.clone(),
                        spawned: spawn_selection_from_mutation(&mutation, &spawned),
                    })
                }
                Err(e) => Err(format!("spawn failed: {e}")),
            }
        }
        Mutation::Unhide(s) => fresh_backend(s.backend, &opencode_runtime)
            .unhide(&s.id)
            .map(|()| MutationOutcome {
                notice: format!("unarchived: {}", s.title),
                spawned: None,
            })
            .map_err(|e| format!("{}: {e}", s.backend.name())),
    }
}

#[cfg(test)]
mod tests {
    use super::{Mutation, run_mutation, spawn_selection_from_mutation};
    use agent_viewer_core::backend::{BackendKind, Status};
    use agent_viewer_core::{Session, SpawnResult};
    use agent_viewer_tui::app::{App, Row};
    use std::path::PathBuf;
    use std::time::Duration;

    fn session(backend: BackendKind, id: &str, hidden: bool) -> Session {
        Session {
            backend,
            id: id.to_string(),
            short_id: None,
            origin: agent_viewer_core::SessionOrigin::Interactive,
            title: id.to_string(),
            cwd: PathBuf::from("/tmp/spawn_selection"),
            git_branch: None,
            status: Status::Done,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden,
            companion: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    fn spawn_mutation_with_hidden_session() -> Mutation {
        let app = App::new(vec![
            session(BackendKind::Codex, "visible", false),
            session(BackendKind::Codex, "archived", true),
            session(BackendKind::Claude, "other_backend", false),
        ]);
        assert!(
            !app.visible()
                .iter()
                .any(|row| matches!(row, Row::Session { id, .. } if id == "archived"))
        );

        Mutation::spawn(
            &app,
            BackendKind::Codex,
            PathBuf::from("/tmp/spawn_selection"),
            "new task".to_string(),
            None,
            42,
            "spawned on codex".to_string(),
        )
    }

    #[test]
    fn spawn_submission_captures_hidden_preexisting_session_ids() {
        let mutation = spawn_mutation_with_hidden_session();
        let Mutation::Spawn {
            preexisting_ids, ..
        } = mutation
        else {
            panic!("expected spawn mutation");
        };

        assert_eq!(
            preexisting_ids,
            ["visible".to_string(), "archived".to_string()]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn spawn_selection_preserves_ids_captured_by_spawn_submission() {
        let mutation = spawn_mutation_with_hidden_session();
        let selection = spawn_selection_from_mutation(
            &mutation,
            &SpawnResult {
                pid: None,
                session_id: Some("new_session".to_string()),
            },
        )
        .expect("spawn mutation produces selection metadata");

        assert_eq!(selection.backend, BackendKind::Codex);
        assert_eq!(selection.session_id.as_deref(), Some("new_session"));
        assert_eq!(selection.cwd, PathBuf::from("/tmp/spawn_selection"));
        assert_eq!(selection.spawned_at_ms, 42);
        assert_eq!(
            selection.preexisting_ids,
            ["visible".to_string(), "archived".to_string()]
                .into_iter()
                .collect()
        );
    }

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
            origin: agent_viewer_core::SessionOrigin::Background,
            title: "probe".to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            git_branch: None,
            status: Status::Working,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            summary: String::new(),
            pid: Some(pid),
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
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
