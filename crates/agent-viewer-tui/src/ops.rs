//! Blocking backend mutations (stop/remove/rename/hide), each run to completion on a
//! `MutationRunner` worker thread with all data owned (Send).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use agent_viewer_core::agent_runner::AgentRunnerBackend;
use agent_viewer_core::backend::{Backend, BackendKind};
use agent_viewer_core::claude::{ClaudeBackend, ensure_trusted};
use agent_viewer_core::codex::CodexBackend;
use agent_viewer_core::group::project_root;
use agent_viewer_core::router::RouterOutcome;
use agent_viewer_core::spawn::now_ms;
use agent_viewer_core::{Session, SpawnResult, Status, ViewerDb, default_codex_home};
use agent_viewer_tui::app::App;
use agent_viewer_tui::mutations::{MutationOutcome, SpawnSelection};
use agent_viewer_tui::shared_listing::{
    SpawnDirectoryMode, SpawnTarget, TargetRequest, authoritative_target, invalidate_backend_scope,
};

pub(crate) struct AttachPlan {
    pub(crate) session: Session,
    pub(crate) command: std::process::Command,
    pub(crate) release: Option<std::process::Command>,
}

impl AttachPlan {
    pub(crate) fn into_parts(
        mut self,
    ) -> (
        Session,
        std::process::Command,
        Option<std::process::Command>,
    ) {
        let command = std::mem::replace(
            &mut self.command,
            std::process::Command::new("agent-viewer-unused-command"),
        );
        let release = self.release.take();
        (self.session.clone(), command, release)
    }

    pub(crate) fn into_release(mut self) -> Option<std::process::Command> {
        self.release.take()
    }
}

impl Drop for AttachPlan {
    fn drop(&mut self) {
        if let Some(mut release) = self.release.take() {
            release
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let _ = release.status();
        }
    }
}

/// A blocking backend mutation, run on a worker thread with all data owned (Send).
pub(crate) enum Mutation {
    Stop(TargetRequest),
    Remove {
        request: TargetRequest,
        require_finished: bool,
    },
    Rename(TargetRequest, String),
    Hide(TargetRequest),
    Unhide(TargetRequest),
    /// Spawn a new session. On the runner, not the key path: a codex spawn now talks to the
    /// app-server daemon and may start one, so it is a multi-second blocking call on a bad day
    /// and would freeze the composer if it ran inline like it used to.
    Spawn {
        backend: BackendKind,
        target: SpawnTarget,
        task: String,
        model: Option<String>,
        spawned_at_ms: i64,
        preexisting_ids: HashSet<String>,
        notice: String,
    },
    /// Spawn through `agent-router`: the router classifies the task, weighs weekly usage
    /// headroom, and dispatches to whichever backend won, so the provider is only known once
    /// it returns. Auto is not a `Backend` (it lists nothing), which is why this variant does
    /// not go through the trait at all.
    ///
    /// Deliberately carries NO timestamps: the interval a routed row is matched against is
    /// stamped on the worker, around the router call itself (see `run_spawn_auto`).
    SpawnAuto {
        target: SpawnTarget,
        task: String,
        /// Per-backend session ids as of submission: the winning provider's set is the one the
        /// row selection needs, and which that is cannot be known until the router answers.
        preexisting_ids: HashMap<BackendKind, HashSet<String>>,
    },
}

impl Mutation {
    pub(crate) fn spawn_auto(app: &App, target: SpawnTarget, task: String) -> Self {
        Self::SpawnAuto {
            target,
            task,
            preexisting_ids: [BackendKind::Codex, BackendKind::Claude]
                .into_iter()
                .map(|backend| (backend, app.session_ids_for_backend(backend)))
                .collect(),
        }
    }

    pub(crate) fn spawn(
        app: &App,
        backend: BackendKind,
        target: SpawnTarget,
        task: String,
        model: Option<String>,
        spawned_at_ms: i64,
        notice: String,
    ) -> Self {
        Self::Spawn {
            backend,
            target,
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
        target,
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
        job_name: None,
        cwd: target.displayed_directory().to_path_buf(),
        // A direct spawn is one instant: submission and completion are the same stamp.
        submitted_at_ms: *spawned_at_ms,
        spawned_at_ms: *spawned_at_ms,
        preexisting_ids: preexisting_ids.clone(),
    })
}

/// A fresh backend instance for a worker thread.
fn fresh_backend(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Codex => Box::new(CodexBackend::new(default_codex_home())),
        BackendKind::Claude => Box::new(ClaudeBackend::new()),
        BackendKind::AgentRunner => Box::new(AgentRunnerBackend::new()),
    }
}

fn target_failure(resolution: agent_viewer_tui::shared_listing::TargetResolution) -> String {
    resolution
        .notice()
        .unwrap_or("target resolution failed")
        .to_string()
}

pub(crate) fn resolve_attach_with_backend(
    backend: &mut dyn Backend,
    request: TargetRequest,
) -> Result<AttachPlan, String> {
    let session = authoritative_target(backend, &request).map_err(target_failure)?;
    if !backend.capabilities_for(&session).attach {
        return Err(format!(
            "{} does not support attach",
            session.backend.name()
        ));
    }
    let claude_fallback = session.backend == BackendKind::Claude
        && session.short_id.as_deref().unwrap_or_default().is_empty();
    if claude_fallback {
        let config = agent_viewer_core::home_dir().join(".claude.json");
        let _ = ensure_trusted(&config, &session.cwd);
    }
    let command = backend
        .attach_command(&session)
        .map_err(|refusal| refusal.reason)?;
    Ok(AttachPlan {
        session,
        command,
        release: None,
    })
}

pub(crate) fn resolve_attach(request: TargetRequest) -> Result<AttachPlan, String> {
    if request.backend() == BackendKind::AgentRunner {
        let mut backend = AgentRunnerBackend::new();
        let session = authoritative_target(&mut backend, &request).map_err(target_failure)?;
        let prepared = backend
            .prepare_attach(&session)
            .map_err(|refusal| refusal.reason)?;
        let (command, release) = prepared.into_commands();
        return Ok(AttachPlan {
            session,
            command,
            release: Some(release),
        });
    }
    let mut backend = fresh_backend(request.backend());
    resolve_attach_with_backend(backend.as_mut(), request)
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

fn run_with_fresh_backend<F>(request: TargetRequest, action: F) -> Result<MutationOutcome, String>
where
    F: FnOnce(&dyn Backend, &Session) -> Result<MutationOutcome, String>,
{
    let mut backend = fresh_backend(request.backend());
    let db = ViewerDb::open_default().ok();
    run_targeted(backend.as_mut(), db.as_ref(), &request, action)
}

/// A session we just stopped can briefly still resolve as `Working`/`NeedsInput`: `claude stop`
/// (and the codex equivalent) reports success once the interrupt is accepted, but the
/// session's own status source (Claude's `claude agents --json`, a rollout file, etc.) settles a
/// beat later. Poll `authoritative_target` again within this bounded window before concluding
/// the session is genuinely active again, rather than refusing on the very next stale read.
const REMOVE_SETTLE_TIMEOUT: Duration = Duration::from_millis(300);
const REMOVE_SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(60);

fn run_remove(
    backend: &mut dyn Backend,
    db: Option<&ViewerDb>,
    request: &TargetRequest,
    require_finished: bool,
) -> Result<MutationOutcome, String> {
    let result = remove_settled_target(backend, request, require_finished);
    invalidate_backend_scope(db, backend);
    result
}

fn remove_settled_target(
    backend: &mut dyn Backend,
    request: &TargetRequest,
    require_finished: bool,
) -> Result<MutationOutcome, String> {
    let deadline = Instant::now() + REMOVE_SETTLE_TIMEOUT;
    let mut session = authoritative_target(backend, request).map_err(target_failure)?;
    while matches!(session.status, Status::Working | Status::NeedsInput { .. })
        && Instant::now() < deadline
    {
        std::thread::sleep(REMOVE_SETTLE_POLL_INTERVAL);
        session = authoritative_target(backend, request).map_err(target_failure)?;
    }
    if !backend.capabilities_for(&session).delete {
        return Err(format!(
            "{} does not support remove",
            session.backend.name()
        ));
    }
    let freshly_active = matches!(session.status, Status::Working | Status::NeedsInput { .. });
    if freshly_active || require_finished && !session.status.is_finished() {
        return Err(format!(
            "remove refused: {} session became active",
            session.backend.name()
        ));
    }
    if let Some(pid) = session.pid.filter(|_| !session.daemon_hosted) {
        let _ = agent_viewer_core::spawn::terminate(pid, session.backend.name());
    }
    match backend.remove(&session) {
        Ok(()) => Ok(MutationOutcome {
            notice: format!("removed: {}", session.title),
            spawned: None,
        }),
        Err(agent_viewer_core::error::Error::Unsupported(name)) => {
            Err(format!("{name} does not support remove"))
        }
        Err(error) => Err(format!("remove failed: {error}")),
    }
}

fn run_spawn_at_directory(
    backend: &mut dyn Backend,
    db: Option<&ViewerDb>,
    mutation: &Mutation,
    directory: std::path::PathBuf,
) -> Result<MutationOutcome, String> {
    let Mutation::Spawn {
        backend: backend_kind,
        task,
        model,
        spawned_at_ms,
        notice,
        ..
    } = mutation
    else {
        unreachable!();
    };
    let result = match backend.spawn(&directory, task, model.as_deref(), None) {
        Ok(spawned) => {
            if let Some(pid) = spawned.pid
                && let Some(db) = db
            {
                let _ = db.record_spawn(*backend_kind, &directory, pid, *spawned_at_ms);
            }
            let mut selection = spawn_selection_from_mutation(mutation, &spawned);
            if let Some(selection) = &mut selection {
                selection.cwd = directory;
            }
            Ok(MutationOutcome {
                notice: notice.clone(),
                spawned: selection,
            })
        }
        Err(error) => Err(format!("spawn failed: {error}")),
    };
    invalidate_backend_scope(db, backend);
    result
}

fn run_spawn_with_backend(
    backend: &mut dyn Backend,
    db: Option<&ViewerDb>,
    mutation: &Mutation,
) -> Result<MutationOutcome, String> {
    let Mutation::Spawn { target, .. } = mutation else {
        unreachable!();
    };
    let directory = match target {
        SpawnTarget::Session { request, mode, .. } => {
            let session = authoritative_target(backend, request).map_err(target_failure)?;
            match mode {
                SpawnDirectoryMode::WorkingDirectory => session.cwd,
                SpawnDirectoryMode::ProjectRoot => project_root(&session.cwd),
            }
        }
        SpawnTarget::ExplicitDirectory(directory) => directory.clone(),
    };
    run_spawn_at_directory(backend, db, mutation, directory)
}

/// The directory a spawn target resolves to, for a spawn path with no backend of its own.
/// A session target is resolved against the backend that OWNS the row, which is the only one
/// that can answer for it.
fn spawn_directory(target: &SpawnTarget) -> Result<std::path::PathBuf, String> {
    match target {
        SpawnTarget::ExplicitDirectory(directory) => Ok(directory.clone()),
        SpawnTarget::Session { request, mode, .. } => {
            let mut owner = fresh_backend(request.backend());
            let session = authoritative_target(owner.as_mut(), request).map_err(target_failure)?;
            Ok(match mode {
                SpawnDirectoryMode::WorkingDirectory => session.cwd,
                SpawnDirectoryMode::ProjectRoot => project_root(&session.cwd),
            })
        }
    }
}

/// Route one task through `agent-router` and turn its decision into the footer notice plus the
/// row selection for whichever backend won. Every router failure (missing binary, non-zero
/// exit, timeout, unreadable json) is an Err the user reads: nothing is spawned, and there is
/// no fallback to a guessed provider.
///
/// `route` is a parameter so the stamping rule below is testable without a router on PATH.
fn run_spawn_auto(
    target: &SpawnTarget,
    task: &str,
    preexisting_ids: &HashMap<BackendKind, HashSet<String>>,
    db: Option<&ViewerDb>,
    route: impl FnOnce(&std::path::Path, &str) -> Result<RouterOutcome, String>,
) -> Result<MutationOutcome, String> {
    let directory = spawn_directory(target)?;
    // BOTH stamps are kept, because the routed job is created somewhere between them and the
    // fallback has to bracket that whole interval. The classifier call plus the winning backend's
    // own spawn can burn most of the router's 180s deadline, and the router then polls seconds
    // longer for a job id it may never resolve. Matched against the return stamp alone the row is
    // already too old (that window reaches only 2s back); matched against the invocation alone the
    // window can expire before the row appears.
    let submitted_at_ms = now_ms();
    let outcome = route(&directory, task)?;
    let spawned_at_ms = now_ms();
    // The winning provider's cached listing predates the job it now owns: fence it, same as a
    // direct spawn does, so the next tick refetches instead of waiting out the freshness window.
    let winner = fresh_backend(outcome.provider);
    invalidate_backend_scope(db, winner.as_ref());
    Ok(MutationOutcome {
        notice: outcome.notice(),
        spawned: Some(SpawnSelection {
            backend: outcome.provider,
            session_id: outcome.job_id,
            job_name: outcome.job_name,
            cwd: directory,
            submitted_at_ms,
            spawned_at_ms,
            preexisting_ids: preexisting_ids
                .get(&outcome.provider)
                .cloned()
                .unwrap_or_default(),
        }),
    })
}

/// Run one mutation to completion, applying its viewer-DB follow-up against a fresh
/// connection so the render loop never blocks. Returns the user-facing notice and any
/// successful spawn identity needed by the UI.
pub(crate) fn run_mutation(m: Mutation) -> Result<MutationOutcome, String> {
    match m {
        Mutation::Stop(request) => run_with_fresh_backend(request, |backend, session| {
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
        }),
        Mutation::Remove {
            request,
            require_finished,
        } => {
            let mut backend = fresh_backend(request.backend());
            let db = ViewerDb::open_default().ok();
            run_remove(backend.as_mut(), db.as_ref(), &request, require_finished)
        }
        Mutation::Rename(request, name) => run_with_fresh_backend(request, |backend, session| {
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
        }),
        Mutation::Hide(request) => run_with_fresh_backend(request, |backend, session| {
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
        }),
        Mutation::Unhide(request) => run_with_fresh_backend(request, |backend, session| {
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
        }),
        Mutation::SpawnAuto {
            target,
            task,
            preexisting_ids,
        } => {
            let db = ViewerDb::open_default().ok();
            run_spawn_auto(
                &target,
                &task,
                &preexisting_ids,
                db.as_ref(),
                agent_viewer_core::router::route,
            )
        }
        mutation @ Mutation::Spawn { .. } => {
            let Mutation::Spawn {
                backend, target, ..
            } = &mutation
            else {
                unreachable!();
            };
            let mut action_backend = fresh_backend(*backend);
            let db = ViewerDb::open_default().ok();
            if let SpawnTarget::Session { request, mode, .. } = target
                && request.backend() != *backend
            {
                let mut authority_backend = fresh_backend(request.backend());
                let directory = match authoritative_target(authority_backend.as_mut(), request) {
                    Ok(session) => match mode {
                        SpawnDirectoryMode::WorkingDirectory => session.cwd,
                        SpawnDirectoryMode::ProjectRoot => project_root(&session.cwd),
                    },
                    Err(resolution) => return Err(target_failure(resolution)),
                };
                run_spawn_at_directory(action_backend.as_mut(), db.as_ref(), &mutation, directory)
            } else {
                run_spawn_with_backend(action_backend.as_mut(), db.as_ref(), &mutation)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Mutation, RouterOutcome, run_remove, run_spawn_auto, run_spawn_with_backend,
        spawn_selection_from_mutation,
    };
    use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Status};
    use agent_viewer_core::claude::ClaudeBackend;
    use agent_viewer_core::spawn::now_ms;
    use agent_viewer_core::{ListingCacheClaim, Session, SpawnResult, ViewerDb};
    use agent_viewer_tui::app::{App, Row};
    use agent_viewer_tui::shared_listing::{SpawnDirectoryMode, SpawnTarget, TargetRequest};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// A routed row with no job id is matched by `match_spawn`'s cwd + creation-time rule, and
    /// that window closes 30s after the stamp. Classification plus dispatch can run for most of
    /// the router's 180s deadline, so the stamp must be taken when the decision LANDS; taken at
    /// submission it can be long expired before the row it was meant to select exists.
    #[test]
    fn a_routed_spawn_is_stamped_when_the_router_returns_not_when_it_was_submitted() {
        let submitted_at = now_ms();
        let routing_ms = 60;

        let outcome = run_spawn_auto(
            &SpawnTarget::ExplicitDirectory(PathBuf::from("/tmp/routed_spawn")),
            "a routed task",
            &HashMap::new(),
            None,
            |_directory, _task| {
                std::thread::sleep(Duration::from_millis(routing_ms));
                Ok(RouterOutcome {
                    provider: BackendKind::Codex,
                    model: None,
                    effort: Some("xhigh".to_string()),
                    job_id: None,
                    job_name: Some("a routed task".to_string()),
                    gates: Vec::new(),
                    rationale: String::new(),
                    claude_weekly_pct: 0.0,
                    codex_weekly_pct: 0.0,
                })
            },
        )
        .expect("a routed decision is a successful mutation");

        let selection = outcome.spawned.expect("a routed spawn selects its row");
        assert_eq!(selection.backend, BackendKind::Codex);
        assert!(
            selection.spawned_at_ms >= submitted_at + routing_ms as i64,
            "the stamp must be taken after routing: {} is not at least {}",
            selection.spawned_at_ms,
            submitted_at + routing_ms as i64
        );
    }

    /// A direct spawn invalidates its backend's shared listing cache so the new row appears on
    /// the next tick instead of waiting out the freshness window; a routed spawn owes the
    /// winning provider the same. The scope is seeded exactly as production resolves it
    /// (`fresh_backend`), because invalidation is keyed by the backend's own scope string.
    #[test]
    fn a_successful_routed_spawn_invalidates_the_winning_provider_listing() {
        let directory = tempfile::tempdir().expect("temporary viewer state directory");
        let db = ViewerDb::open(&directory.path().join("viewer.sqlite"))
            .expect("temporary viewer database");
        let scope = ClaudeBackend::new()
            .listing_scope()
            .expect("claude advertises a listing scope");

        // A listing worker holds a live lease; without invalidation a re-claim only observes it.
        let claim = db
            .claim_listing_refresh(Some(&scope), None, 1_000, 60_000, 60_000)
            .expect("claiming an empty scope");
        let ListingCacheClaim::Claimed(lease) = claim else {
            panic!("the first claim must win the lease, got {claim:?}");
        };

        run_spawn_auto(
            &SpawnTarget::ExplicitDirectory(PathBuf::from("/tmp/routed_invalidate")),
            "a routed task",
            &HashMap::new(),
            Some(&db),
            |_directory, _task| {
                Ok(RouterOutcome {
                    provider: BackendKind::Claude,
                    model: None,
                    effort: None,
                    job_id: Some("routed-short-id".to_string()),
                    job_name: None,
                    gates: Vec::new(),
                    rationale: String::new(),
                    claude_weekly_pct: 0.0,
                    codex_weekly_pct: 0.0,
                })
            },
        )
        .expect("a routed decision is a successful mutation");

        let reclaim = db
            .claim_listing_refresh(Some(&scope), None, 2_000, 60_000, 60_000)
            .expect("re-claiming after the routed spawn");
        match reclaim {
            ListingCacheClaim::Claimed(next) => assert!(
                next.next_published_generation() > lease.next_published_generation(),
                "the routed spawn must bump the generation: {} is not past {}",
                next.next_published_generation(),
                lease.next_published_generation()
            ),
            other => panic!(
                "the routed spawn must clear the lease so the next tick refetches, got {other:?}"
            ),
        }
    }

    /// The routed job is created somewhere INSIDE the router call, not at its return: the router
    /// polls for the winning backend's job id for up to ~10s after it dispatched, and reports no
    /// id when that resolution misses. The row must then be selected by cwd + creation time, and
    /// against a single return stamp its creation is already outside the 2s that window reaches
    /// backwards. So the selection window opens at the instant the router was invoked.
    #[test]
    fn a_routed_selection_window_opens_when_the_router_was_invoked() {
        let before_call = now_ms();
        let routing_ms = 60;

        let outcome = run_spawn_auto(
            &SpawnTarget::ExplicitDirectory(PathBuf::from("/tmp/routed_window")),
            "a routed task",
            &HashMap::new(),
            None,
            |_directory, _task| {
                std::thread::sleep(Duration::from_millis(routing_ms));
                Ok(RouterOutcome {
                    provider: BackendKind::Claude,
                    model: None,
                    effort: None,
                    // The short-id resolution missed, so the row is matched by cwd + time.
                    job_id: None,
                    job_name: Some("a routed task".to_string()),
                    gates: Vec::new(),
                    rationale: String::new(),
                    claude_weekly_pct: 0.0,
                    codex_weekly_pct: 0.0,
                })
            },
        )
        .expect("a routed decision is a successful mutation");

        let selection = outcome.spawned.expect("a routed spawn selects its row");
        assert!(
            selection.submitted_at_ms >= before_call,
            "the window must open no earlier than the submission: {} is before {}",
            selection.submitted_at_ms,
            before_call
        );
        assert!(
            selection.spawned_at_ms - selection.submitted_at_ms >= routing_ms as i64,
            "the window must span the whole router call: {} to {} is shorter than {routing_ms}ms",
            selection.submitted_at_ms,
            selection.spawned_at_ms
        );
    }

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
            subagent: false,
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
        // A direct spawn knows exactly when it happened, so its window is that one instant.
        assert_eq!(selection.submitted_at_ms, 42);
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
            _effort: Option<&str>,
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
            subagent: false,
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
            _effort: Option<&str>,
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

    struct RecordingRemoveBackend {
        session: Session,
        removed_ids: RefCell<Vec<String>>,
    }

    impl Backend for RecordingRemoveBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                delete: true,
                ..Capabilities::none()
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
            _effort: Option<&str>,
        ) -> agent_viewer_core::Result<SpawnResult> {
            unreachable!("spawn is not exercised by remove")
        }

        fn remove(&self, session: &Session) -> agent_viewer_core::Result<()> {
            self.removed_ids.borrow_mut().push(session.id.clone());
            Ok(())
        }

        fn attach_command(
            &self,
            _session: &Session,
        ) -> Result<std::process::Command, agent_viewer_core::AttachRefusal> {
            unreachable!("attach is not exercised by remove")
        }
    }

    #[test]
    fn remove_refuses_a_session_that_became_active_after_a_removable_row_was_cached() {
        for (cached_status, fresh_status, require_finished) in [
            (Status::Done, Status::Working, true),
            (Status::Done, Status::needs_input(), true),
            (Status::Idle, Status::Working, false),
            (Status::Idle, Status::needs_input(), false),
            (Status::Unknown, Status::Working, false),
            (Status::Unknown, Status::needs_input(), false),
        ] {
            let mut cached = session(BackendKind::Claude, "stale-status", false);
            cached.status = cached_status;
            let request = TargetRequest::from(&cached);
            let mut fresh = cached;
            fresh.status = fresh_status;
            let mut backend = RecordingRemoveBackend {
                session: fresh,
                removed_ids: RefCell::new(Vec::new()),
            };

            let result = run_remove(&mut backend, None, &request, require_finished);

            assert!(
                result.is_err(),
                "a fresh active session must refuse a remove requested from a stale removable row"
            );
            assert!(
                backend.removed_ids.borrow().is_empty(),
                "remove must not reach the backend for a freshly active session"
            );
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

        let result = run_remove(&mut backend, None, &request, false);

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

#[cfg(test)]
mod attach_resolution_tests {
    use super::resolve_attach_with_backend;
    use agent_viewer_core::backend::{Backend, BackendKind, Capabilities, Status};
    use agent_viewer_core::{AttachRefusal, Session, SpawnResult};
    use agent_viewer_tui::shared_listing::TargetRequest;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    fn session(id: &str, title: &str) -> Session {
        Session {
            backend: BackendKind::Claude,
            id: id.to_string(),
            short_id: Some(id.to_string()),
            origin: agent_viewer_core::SessionOrigin::Background,
            title: title.to_string(),
            cwd: PathBuf::from(format!("/tmp/{title}")),
            git_branch: None,
            status: Status::Done,
            created_at_ms: 0,
            updated_at_ms: 0,
            hidden: false,
            companion: false,
            subagent: false,
            summary: String::new(),
            pid: None,
            rollout_path: None,
            pr_refs: Vec::new(),
            daemon_hosted: false,
        }
    }

    struct RecordingAttachBackend {
        sessions: Vec<Session>,
        attach_allowed: bool,
        list_calls: usize,
        command_sessions: RefCell<Vec<Session>>,
    }

    impl RecordingAttachBackend {
        fn new(sessions: Vec<Session>, attach_allowed: bool) -> Self {
            Self {
                sessions,
                attach_allowed,
                list_calls: 0,
                command_sessions: RefCell::new(Vec::new()),
            }
        }
    }

    impl Backend for RecordingAttachBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Claude
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                attach: self.attach_allowed,
                ..Capabilities::none()
            }
        }

        fn capabilities_for(&self, _session: &Session) -> Capabilities {
            self.capabilities()
        }

        fn list(&mut self) -> agent_viewer_core::Result<Vec<Session>> {
            self.list_calls += 1;
            Ok(self.sessions.clone())
        }

        fn spawn(
            &self,
            _dir: &Path,
            _task: &str,
            _model: Option<&str>,
            _effort: Option<&str>,
        ) -> agent_viewer_core::Result<SpawnResult> {
            unreachable!("spawn is not exercised by attach resolution")
        }

        fn attach_command(
            &self,
            session: &Session,
        ) -> Result<std::process::Command, AttachRefusal> {
            self.command_sessions.borrow_mut().push(session.clone());
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "sleep 30"]);
            Ok(command)
        }
    }

    #[test]
    fn attach_resolution_builds_the_plan_from_the_fresh_authoritative_session() {
        let displayed = session("target", "displayed_stale");
        let fresh = session("target", "authority_fresh");
        let request = TargetRequest::from(&displayed);
        let mut backend = RecordingAttachBackend::new(vec![fresh.clone()], true);

        let plan =
            resolve_attach_with_backend(&mut backend, request).expect("attach plan resolves");

        assert_eq!(backend.list_calls, 1);
        assert_eq!(*backend.command_sessions.borrow(), vec![fresh.clone()]);
        assert_eq!(plan.session, fresh);
    }

    #[test]
    fn missing_or_freshly_refused_attach_never_builds_a_command() {
        let displayed = session("target", "displayed");
        let request = TargetRequest::from(&displayed);
        let mut missing = RecordingAttachBackend::new(Vec::new(), true);

        let missing_result = resolve_attach_with_backend(&mut missing, request.clone());

        assert_eq!(
            missing_result.err().as_deref(),
            Some("claude session is no longer available")
        );
        assert_eq!(missing.list_calls, 1);
        assert!(missing.command_sessions.borrow().is_empty());

        let fresh = session("target", "authority_refused");
        let mut refused = RecordingAttachBackend::new(vec![fresh], false);

        let refused_result = resolve_attach_with_backend(&mut refused, request);

        assert_eq!(
            refused_result.err().as_deref(),
            Some("claude does not support attach")
        );
        assert_eq!(refused.list_calls, 1);
        assert!(refused.command_sessions.borrow().is_empty());
    }
}
