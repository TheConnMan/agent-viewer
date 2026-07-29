//! Blocking backend mutations (stop/remove/rename/hide), each run to completion on a
//! `MutationRunner` worker thread with all data owned (Send).

use std::collections::HashSet;

use agent_viewer_core::backend::{Backend, BackendKind};
use agent_viewer_core::claude::ClaudeBackend;
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::opencode::{OpencodeBackend, OpencodeRuntime};
use agent_viewer_core::{Session, SpawnResult, ViewerDb, default_codex_home};
use agent_viewer_tui::app::App;
use agent_viewer_tui::mutations::{MutationOutcome, SpawnSelection};
use agent_viewer_tui::shared_listing::{
    TargetRequest, authoritative_target, invalidate_backend_scope,
};

/// A blocking backend mutation, run on a worker thread with all data owned (Send).
pub(crate) enum Mutation {
    Stop(TargetRequest),
    Remove(TargetRequest),
    Rename(TargetRequest, String),
    Hide(TargetRequest),
    Unhide(TargetRequest),
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

/// A fresh backend instance for a worker thread.
fn fresh_backend(kind: BackendKind, opencode_runtime: &OpencodeRuntime) -> Box<dyn Backend> {
    match kind {
        BackendKind::Codex => Box::new(CodexBackend::new(default_codex_home())),
        BackendKind::Claude => Box::new(ClaudeBackend::new()),
        BackendKind::Opencode => Box::new(OpencodeBackend::with_runtime(opencode_runtime.clone())),
    }
}

fn target_failure(resolution: agent_viewer_tui::shared_listing::TargetResolution) -> String {
    resolution
        .notice()
        .unwrap_or("target resolution failed")
        .to_string()
}

fn run_targeted<F>(
    backend: &mut dyn Backend,
    db: Option<&ViewerDb>,
    request: &TargetRequest,
    action: F,
) -> Result<MutationOutcome, String>
where
    F: FnOnce(&dyn Backend, &Session) -> Result<MutationOutcome, String>,
{
    let result = match authoritative_target(backend, request) {
        Ok(session) => action(backend, &session),
        Err(resolution) => Err(target_failure(resolution)),
    };
    invalidate_backend_scope(db, backend);
    result
}

fn run_with_fresh_backend<F>(
    request: TargetRequest,
    opencode_runtime: &OpencodeRuntime,
    action: F,
) -> Result<MutationOutcome, String>
where
    F: FnOnce(&dyn Backend, &Session) -> Result<MutationOutcome, String>,
{
    let mut backend = fresh_backend(request.backend(), opencode_runtime);
    let db = ViewerDb::open_default().ok();
    run_targeted(backend.as_mut(), db.as_ref(), &request, action)
}

fn run_remove(
    backend: &mut dyn Backend,
    db: Option<&ViewerDb>,
    request: &TargetRequest,
) -> Result<MutationOutcome, String> {
    run_targeted(backend, db, request, |backend, session| {
        if !backend.capabilities_for(session).delete {
            return Err(format!(
                "{} does not support remove",
                session.backend.name()
            ));
        }
        if let Some(pid) = session.pid.filter(|_| !session.daemon_hosted) {
            let _ = agent_viewer_core::spawn::terminate(pid, session.backend.name());
        }
        match backend.remove(session) {
            Ok(()) => Ok(MutationOutcome {
                notice: format!("removed: {}", session.title),
                spawned: None,
            }),
            Err(agent_viewer_core::error::Error::Unsupported(name)) => {
                Err(format!("{name} does not support remove"))
            }
            Err(error) => Err(format!("remove failed: {error}")),
        }
    })
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
        Mutation::Stop(request) => {
            run_with_fresh_backend(request, &opencode_runtime, |backend, session| {
                if !backend.capabilities_for(session).stop {
                    return Err(format!("{} does not support stop", session.backend.name()));
                }
                backend
                    .stop(session)
                    .map(|()| MutationOutcome {
                        notice: format!("stopped: {}", session.title),
                        spawned: None,
                    })
                    .map_err(|error| format!("stop failed: {error}"))
            })
        }
        Mutation::Remove(request) => {
            let mut backend = fresh_backend(request.backend(), &opencode_runtime);
            let db = ViewerDb::open_default().ok();
            run_remove(backend.as_mut(), db.as_ref(), &request)
        }
        Mutation::Rename(request, name) => {
            run_with_fresh_backend(request, &opencode_runtime, |backend, session| {
                if !backend.capabilities_for(session).rename {
                    return Err(format!(
                        "{} does not support rename",
                        session.backend.name()
                    ));
                }
                backend
                    .rename(session, &name)
                    .map(|()| MutationOutcome {
                        notice: format!("renamed {}", session.backend.name()),
                        spawned: None,
                    })
                    .map_err(|error| format!("rename failed: {error}"))
            })
        }
        Mutation::Hide(request) => {
            run_with_fresh_backend(request, &opencode_runtime, |backend, session| {
                if !backend.capabilities_for(session).archive {
                    return Err(format!("{} does not support hide", session.backend.name()));
                }
                backend
                    .hide(&session.id)
                    .map(|()| MutationOutcome {
                        notice: format!("archived: {}", session.title),
                        spawned: None,
                    })
                    .map_err(|error| format!("{}: {error}", session.backend.name()))
            })
        }
        Mutation::Unhide(request) => {
            run_with_fresh_backend(request, &opencode_runtime, |backend, session| {
                if !backend.capabilities_for(session).archive {
                    return Err(format!(
                        "{} does not support unhide",
                        session.backend.name()
                    ));
                }
                backend
                    .unhide(&session.id)
                    .map(|()| MutationOutcome {
                        notice: format!("unarchived: {}", session.title),
                        spawned: None,
                    })
                    .map_err(|error| format!("{}: {error}", session.backend.name()))
            })
        }
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
            let action_backend = fresh_backend(*backend, &opencode_runtime);
            let db = ViewerDb::open_default().ok();
            let result = match action_backend.spawn(dir, task, model.as_deref()) {
                Ok(spawned) => {
                    if let Some(pid) = spawned.pid
                        && let Some(db) = &db
                    {
                        let _ = db.record_spawn(*backend, dir, pid, *spawned_at_ms);
                    }
                    Ok(MutationOutcome {
                        notice: notice.clone(),
                        spawned: spawn_selection_from_mutation(&mutation, &spawned),
                    })
                }
                Err(error) => Err(format!("spawn failed: {error}")),
            };
            invalidate_backend_scope(db.as_ref(), action_backend.as_ref());
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mutation, run_remove, run_spawn_with_backend, spawn_selection_from_mutation};
    use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Status};
    use agent_viewer_core::{Session, SpawnResult};
    use agent_viewer_tui::app::{App, Row};
    use agent_viewer_tui::shared_listing::{SpawnDirectoryMode, SpawnTarget, TargetRequest};
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
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
            SpawnTarget::ExplicitDirectory(PathBuf::from("/tmp/spawn_selection")),
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

    struct RecordingSpawnBackend {
        sessions: Vec<Session>,
        list_calls: usize,
        spawn_directories: RefCell<Vec<PathBuf>>,
    }

    impl RecordingSpawnBackend {
        fn with_sessions(sessions: Vec<Session>) -> Self {
            Self {
                sessions,
                list_calls: 0,
                spawn_directories: RefCell::new(Vec::new()),
            }
        }
    }

    impl Backend for RecordingSpawnBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Codex
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                spawn: true,
                ..Capabilities::none()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            self.list_calls += 1;
            Ok(self.sessions.clone())
        }

        fn spawn(
            &self,
            dir: &Path,
            _task: &str,
            _model: Option<&str>,
        ) -> agent_viewer_core::Result<SpawnResult> {
            self.spawn_directories.borrow_mut().push(dir.to_path_buf());
            Ok(SpawnResult {
                pid: None,
                session_id: Some("spawned_session".to_string()),
            })
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attach is not exercised by spawn")
        }
    }

    fn session_at(id: &str, cwd: impl Into<PathBuf>) -> Session {
        let mut session = session(BackendKind::Codex, id, false);
        session.cwd = cwd.into();
        session
    }

    fn spawn_mutation(target: SpawnTarget) -> Mutation {
        Mutation::Spawn {
            backend: BackendKind::Codex,
            target,
            task: "new task".to_string(),
            model: None,
            spawned_at_ms: 42,
            preexisting_ids: ["displayed_other_session".to_string()]
                .into_iter()
                .collect(),
            notice: "spawned on codex".to_string(),
        }
    }

    #[test]
    fn session_working_directory_spawn_uses_fresh_authoritative_cwd() {
        let displayed = session_at("target", "/displayed/stale");
        let fresh = session_at("target", "/authority/fresh");
        let target = SpawnTarget::Session {
            request: TargetRequest::from(&displayed),
            mode: SpawnDirectoryMode::WorkingDirectory,
            displayed_directory: displayed.cwd.clone(),
        };
        let mutation = spawn_mutation(target);
        let mut backend = RecordingSpawnBackend::with_sessions(vec![fresh]);

        let outcome =
            run_spawn_with_backend(&mut backend, None, &mutation).expect("spawn succeeds");

        assert_eq!(backend.list_calls, 1);
        assert_eq!(
            backend.spawn_directories.into_inner(),
            vec![PathBuf::from("/authority/fresh")]
        );
        assert_eq!(
            outcome.spawned.expect("spawn selection").cwd,
            PathBuf::from("/authority/fresh")
        );
    }

    #[test]
    fn session_project_spawn_recomputes_project_root_from_fresh_cwd() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let repository = directory.path().join("fresh_repository");
        std::fs::create_dir_all(repository.join(".git")).expect("git marker");
        let fresh_cwd = repository.join("new").join("nested");
        std::fs::create_dir_all(&fresh_cwd).expect("fresh working directory");
        let displayed = session_at("target", "/displayed/old_repository");
        let fresh = session_at("target", fresh_cwd);
        let target = SpawnTarget::Session {
            request: TargetRequest::from(&displayed),
            mode: SpawnDirectoryMode::ProjectRoot,
            displayed_directory: displayed.cwd.clone(),
        };
        let mutation = spawn_mutation(target);
        let mut backend = RecordingSpawnBackend::with_sessions(vec![fresh]);

        let outcome =
            run_spawn_with_backend(&mut backend, None, &mutation).expect("spawn succeeds");

        assert_eq!(backend.list_calls, 1);
        assert_eq!(
            backend.spawn_directories.into_inner(),
            vec![repository.clone()]
        );
        assert_eq!(outcome.spawned.expect("spawn selection").cwd, repository);
    }

    #[test]
    fn missing_session_identity_refuses_spawn() {
        let displayed = session_at("missing", "/displayed/stale");
        let target = SpawnTarget::Session {
            request: TargetRequest::from(&displayed),
            mode: SpawnDirectoryMode::WorkingDirectory,
            displayed_directory: displayed.cwd.clone(),
        };
        let mutation = spawn_mutation(target);
        let mut backend = RecordingSpawnBackend::with_sessions(Vec::new());

        let result = run_spawn_with_backend(&mut backend, None, &mutation);

        assert_eq!(
            result,
            Err("codex session is no longer available".to_string())
        );
        assert_eq!(backend.list_calls, 1);
        assert!(backend.spawn_directories.into_inner().is_empty());
    }

    #[test]
    fn explicit_directory_spawn_does_not_list_authority() {
        let directory = PathBuf::from("/explicit/project/header");
        let mutation = spawn_mutation(SpawnTarget::ExplicitDirectory(directory.clone()));
        let mut backend = RecordingSpawnBackend::with_sessions(Vec::new());

        let outcome =
            run_spawn_with_backend(&mut backend, None, &mutation).expect("spawn succeeds");

        assert_eq!(backend.list_calls, 0);
        assert_eq!(
            backend.spawn_directories.into_inner(),
            vec![directory.clone()]
        );
        assert_eq!(outcome.spawned.expect("spawn selection").cwd, directory);
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

    struct RefusingRemoveBackend {
        session: Session,
    }

    impl Backend for RefusingRemoveBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                delete: true,
                ..Capabilities::none()
            }
        }

        fn capabilities_for(&self, session: &Session) -> Capabilities {
            Capabilities {
                delete: session.short_id.is_some(),
                ..self.capabilities()
            }
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            Ok(vec![self.session.clone()])
        }

        fn spawn(
            &self,
            _dir: &std::path::Path,
            _task: &str,
            _model: Option<&str>,
        ) -> agent_viewer_core::Result<SpawnResult> {
            unreachable!("spawn is not exercised by remove")
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attach is not exercised by remove")
        }
    }

    // The defect: `remove` is advertised backend-wide for claude but gated per row on the
    // short id, so an interactive row passed the capability gate, got its process group
    // SIGTERMed, and only then was declined. The session died and stayed in the list.
    #[test]
    fn unsupported_remove_never_terminates_the_live_process() {
        let (dir, mut victim) = claude_named_victim("unsupported");
        let session = claude_session(None, victim.id());
        let request = TargetRequest::from(&session);
        let mut backend = RefusingRemoveBackend { session };

        let result = run_remove(&mut backend, None, &request);

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
