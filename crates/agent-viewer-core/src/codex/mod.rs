pub mod app_server;
pub mod cli;
pub mod pr_scan;
pub mod registry;
pub mod rollout;
pub mod source;
pub mod status;

use crate::backend::{
    Backend, BackendKind, Capabilities, Session, SessionOrigin, SpawnResult, Status,
};
use crate::error::{AttachRefusal, Result};
use crate::platform::Platform;
use registry::Registry;
use status::StatusResolver;
use std::collections::HashSet;
use std::path::Path;

// Enumeration still reads the SQLite registry + rollout files directly; the `codex app-server`
// daemon is used for the two things files cannot express: spawning a thread that stays
// joinable, and joining or interrupting one. See `app_server` and SPEC.md
// "Codex attach/resume".

/// Which spawn path a new session takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnRoute<'a> {
    /// Start the thread on the shared daemon. The only spawn that stays joinable.
    Daemon(&'a app_server::Daemon),
    /// `codex exec`, taken ONLY when explicitly opted into.
    Exec,
    /// No daemon and no opt-in: refuse loudly rather than create an unjoinable session.
    Refuse,
}

/// Opt-in env var for the unjoinable `codex exec` spawn path.
pub const EXEC_SPAWN_ENV: &str = "AGENT_VIEWER_CODEX_EXEC_SPAWN";

/// PURE: the spawn refusal shown when no daemon could be reached, wrapped around `why` (the
/// concrete reason `ensure_daemon` failed). The reason leads, because that is the part the
/// user can act on; a bare "no daemon" makes a missing binary and a wedged one look alike.
pub fn no_daemon_spawn_refusal(why: &str) -> String {
    format!(
        "{why}. A session spawned without a daemon can never be joined, so this spawn was \
         refused; set {EXEC_SPAWN_ENV}=1 to spawn an unjoinable exec session anyway"
    )
}

/// PURE: the daemon wins whenever one is reachable, and there is NO silent fallback.
///
/// `codex exec` runs its app-server IN PROCESS, so an exec-spawned thread can never be joined
/// from outside no matter what attach does later. This used to fall back to exec whenever the
/// daemon path failed for any reason, which made a degraded spawn look identical to a good one
/// in the UI. That is exactly how a daemon poisoned by a deleted cwd stayed invisible for
/// hours on 2026-07-27 while every viewer spawn quietly became unjoinable. A spawn that cannot
/// be joined is now a visible failure, and exec is reachable only through `EXEC_SPAWN_ENV`.
pub fn spawn_route(daemon: Option<&app_server::Daemon>, exec_opt_in: bool) -> SpawnRoute<'_> {
    if exec_opt_in {
        return SpawnRoute::Exec;
    }
    match daemon {
        Some(daemon) => SpawnRoute::Daemon(daemon),
        None => SpawnRoute::Refuse,
    }
}

/// IMPURE: has the operator opted into the unjoinable `codex exec` spawn path?
fn exec_spawn_opt_in() -> bool {
    std::env::var(EXEC_SPAWN_ENV).as_deref() == Ok("1")
}

/// How a row is attached: a true join through the hosting daemon, a plain local resume, or a
/// refusal carrying the reason to show verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachRoute {
    Remote(String),
    Local,
    Refuse(&'static str),
}

/// Shown verbatim in the footer when a row cannot be joined and falls back to the live tail.
/// It names the fork because that is the damage being avoided: a plain resume of a live thread
/// appends a synthesized `TurnAborted{Interrupted}` to the rollout of a session that is still
/// running, and shows the user a forked copy of it.
///
/// This only ever fires for `codex exec` sessions now. Measured live on 2026-07-27: an
/// interactive `codex` launch delegates to the shared daemon (the new session's rollout fd was
/// held by the listening daemon and NOT by the TUI process), so ordinary terminal sessions are
/// daemon-hosted and joinable. `codex exec` hosts its app-server in process, so the sessions
/// bg jobs and plugins create are the unjoinable ones.
const LIVE_TURN_REFUSAL: &str = concat!(
    "session runs inside its own codex exec process and cannot be joined; ",
    "showing a live read-only tail instead of forking it"
);

const PORTABLE_ATTACH_REFUSAL: &str =
    "session completion is not proven, so resuming it could fork a live turn";

/// PURE attach routing:
///   daemon-hosted + a reachable daemon                 -> Remote (a real live join)
///   foreign + mid-turn (Working|NeedsInput) + a pid    -> Refuse (a resume would fork it)
///   everything else                                    -> Local
///
/// A daemon-hosted row whose daemon is gone falls to Local: there is no live thread left to
/// join and none to interrupt either, so opening the transcript beats refusing the row.
pub fn attach_route(session: &Session, daemon: Option<&app_server::Daemon>) -> AttachRoute {
    if session.daemon_hosted {
        return match daemon {
            Some(daemon) => AttachRoute::Remote(app_server::remote_endpoint(daemon)),
            None => AttachRoute::Local,
        };
    }
    let mid_turn = matches!(session.status, Status::Working | Status::NeedsInput { .. });
    if mid_turn && session.pid.is_some() {
        return AttachRoute::Refuse(LIVE_TURN_REFUSAL);
    }
    AttachRoute::Local
}

/// PURE attach routing with the platform's process evidence policy applied.
///
/// Linux retains the measured daemon and fd behavior above. Other platforms refuse attachment
/// because process evidence is unavailable and a plain resume could fork a live turn.
pub fn attach_route_for_platform(
    session: &Session,
    daemon: Option<&app_server::Daemon>,
    platform: Platform,
) -> AttachRoute {
    match platform {
        Platform::Linux => attach_route(session, daemon),
        Platform::Macos | Platform::Windows => AttachRoute::Refuse(PORTABLE_ATTACH_REFUSAL),
    }
}

/// How a row is stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRoute {
    Interrupt,
    Signal(u32),
    Unsupported,
}

/// PURE: the reason to show for a spawn attempt that did not start a turn.
fn spawn_failure_reason(attempt: &app_server::SpawnAttempt) -> String {
    match attempt {
        app_server::SpawnAttempt::Started(thread_id) => format!("thread {thread_id} started"),
        app_server::SpawnAttempt::TurnFailed { thread_id, error } => {
            format!("thread {thread_id} exists but its first turn failed: {error}")
        }
        app_server::SpawnAttempt::NotCreated(error) => error.to_string(),
    }
}

/// PURE: is this row a SUBAGENT thread rather than a process's own primary session?
///
/// `list` folds the registry's serialized `source` enum into two Session fields: `origin` is
/// `Exec` for `source = exec` alone, and `companion` covers `exec` plus every subagent shape.
/// A companion that is not an exec row is therefore exactly a subagent thread.
fn is_subagent_row(session: &Session) -> bool {
    session.companion && session.origin != SessionOrigin::Exec
}

/// PURE stop routing. `daemon_hosted` outranks any pid on the row: the daemon holds the
/// rollout fd of every thread it hosts, so a pid that rode along on such a row is the DAEMON's
/// pid, and signalling it would kill the daemon and every other session inside it.
///
/// A SUBAGENT row is the same hazard one level down and is withheld for the same reason. A
/// `codex exec` parent holds its own rollout fd AND the rollout fd of every subagent thread it
/// spawned (measured live 2026-07-27: pid 2910115 held two), so the fd scan stamps the parent's
/// pid onto the subagent rows too. Signalling from there SIGTERMs the parent's whole process
/// group - the parent session and every sibling subagent - to stop one child. There is no
/// separate process to signal instead, so the row advertises no stop at all.
pub fn stop_route(session: &Session) -> StopRoute {
    if session.daemon_hosted {
        return StopRoute::Interrupt;
    }
    if is_subagent_row(session) {
        return StopRoute::Unsupported;
    }
    match session.pid {
        Some(pid) => StopRoute::Signal(pid),
        None => StopRoute::Unsupported,
    }
}

/// Sandbox args passed to `codex exec` for viewer-spawned sessions.
///
/// This WAS `["--sandbox", "workspace-write"]`. That is wrong for this tool: under
/// `workspace-write` codex mounts `.git` read-only and leaves `network_access` off, so a
/// spawned session cannot fetch, branch, create a worktree, or commit. Verified live against
/// codex-cli 0.145.0 — a write to a file in the workspace exits 0 while a write to `.git/`
/// exits 1, and a real viewer-spawned run ended with "the sandbox mounts `.git` read only, so
/// the required branch and worktree could not be created". Every task this viewer spawns is a
/// git-shaped task on the user's own box, so the sandbox only ever produced silent no-ops.
///
/// Viewer-spawned sessions therefore run unsandboxed with approvals off. This is deliberate
/// and is the escape hatch the previous comment already named. See SPEC.md "Spawn sandbox
/// posture" for the full evidence.
const SANDBOX_ARGS: &[&str] = &["--dangerously-bypass-approvals-and-sandbox"];

/// Build the `codex exec` spawn command (extracted so the model-flag wiring is unit-
/// testable). `model` Some adds `-m <m>`; None uses codex's own default.
fn codex_spawn_command(dir: &Path, task: &str, model: Option<&str>) -> std::process::Command {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("exec").arg("--json").arg("-C").arg(dir);
    cmd.args(SANDBOX_ARGS);
    if let Some(model) = model {
        cmd.arg("-m").arg(model);
    }
    cmd.arg(task);
    cmd
}

/// Run `codex debug models` and parse its catalog. Any failure (spawn error, non-zero
/// exit, unparseable stdout) is a quiet empty Vec — discovery is best-effort.
fn codex_catalog_via_cli() -> Vec<String> {
    let mut cmd = std::process::Command::new("codex");
    cmd.arg("debug").arg("models");
    match crate::spawn::run_with_timeout(cmd, crate::spawn::MODEL_PROBE_TIMEOUT) {
        Some(stdout) => parse_codex_catalog(&stdout),
        None => Vec::new(),
    }
}

/// PURE parse of `codex debug models` JSON stdout. Keep only `visibility == "list"`
/// entries, sort by `priority` ascending (a missing priority sorts last), and return each
/// entry's `slug`. Malformed/missing JSON -> empty Vec (never panics).
pub fn parse_codex_catalog(json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(models) = value.get("models").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    // (priority, slug) with missing priority forced last; stable sort keeps input order
    // among equal priorities.
    let mut listed: Vec<(i64, String)> = models
        .iter()
        .filter(|m| crate::json_str(m, "visibility") == Some("list"))
        .filter_map(|m| {
            let slug = crate::json_str(m, "slug")?.to_string();
            let priority = m
                .get("priority")
                .and_then(|p| p.as_i64())
                .unwrap_or(i64::MAX);
            Some((priority, slug))
        })
        .collect();
    listed.sort_by_key(|(priority, _)| *priority);
    listed.into_iter().map(|(_, slug)| slug).collect()
}

/// Best-effort fallback: the distinct models the codex registry has seen, most-used first.
/// Opens a throwaway read-only connection (this runs on `&self`, so it cannot touch
/// `self.registry`). Any error -> empty Vec.
fn codex_models_via_registry(home: &Path) -> Vec<String> {
    let Ok(db) = registry::find_state_db(home) else {
        return Vec::new();
    };
    let Ok(reg) = Registry::open(&db) else {
        return Vec::new();
    };
    reg.distinct_models().unwrap_or_default()
}

pub struct CodexBackend {
    codex_home: std::path::PathBuf,
    registry: Option<Registry>,
    /// Which `state_N.sqlite` `registry` is open on, so a rollover to state_N+1 is noticed.
    registry_path: Option<std::path::PathBuf>,
    resolver: StatusResolver,
    /// Discovered model catalog, computed once and reused (best-effort; degrades to the
    /// default when both the CLI probe and the registry fallback come up empty).
    models_cache: std::sync::OnceLock<Vec<String>>,
    /// Per-rollout PR refs, held across ticks so each transcript is read once and then only
    /// where it grew.
    pr_scan: pr_scan::PrScanner,
}

impl CodexBackend {
    pub fn new(codex_home: std::path::PathBuf) -> CodexBackend {
        CodexBackend {
            codex_home,
            registry: None,
            registry_path: None,
            resolver: StatusResolver::new(),
            models_cache: std::sync::OnceLock::new(),
            pr_scan: pr_scan::PrScanner::new(),
        }
    }

    /// Query threads, re-resolving the state DB every listing and reopening it once on failure.
    ///
    /// The re-glob is the only thing that notices a Codex upgrade laying down a NEW
    /// `state_N+1.sqlite`: the previous file stays open, stays readable, and keeps answering
    /// with its frozen row set, so a registry resolved once at startup showed no new session
    /// until the viewer was restarted. `find_state_db` is one readdir of `~/.codex`, run once
    /// per listing tick against a scan that already reads thousands of rollout tails.
    fn query_threads(&mut self) -> Result<Vec<registry::Thread>> {
        let db = registry::find_state_db(&self.codex_home)?;
        if self.registry_path.as_deref() != Some(db.as_path()) {
            self.registry = None;
        }
        if self.registry.is_none() {
            self.registry = Some(Registry::open(&db)?);
            self.registry_path = Some(db.clone());
        }
        let opened = self
            .registry
            .as_ref()
            .expect("registry opened immediately above");
        match opened.threads() {
            Ok(threads) => Ok(threads),
            Err(_) => {
                self.registry = None;
                self.registry_path = None;
                let reg = Registry::open(&db)?;
                let threads = reg.threads()?;
                self.registry = Some(reg);
                self.registry_path = Some(db);
                Ok(threads)
            }
        }
    }
}

impl Backend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    fn listing_scope(&self) -> Option<crate::backend::ListingCacheScope> {
        let platform = match crate::platform::current_platform() {
            Platform::Linux => "linux",
            Platform::Macos => "macos",
            Platform::Windows => "windows",
        };
        let key = crate::backend::listing_scope_key(&[
            crate::backend::listing_scope_path(&self.codex_home),
            platform.to_string(),
        ]);
        crate::backend::ListingCacheScope::new(self.kind(), key).ok()
    }

    fn capabilities(&self) -> Capabilities {
        capabilities_for_platform(crate::platform::current_platform())
    }

    fn capabilities_for(&self, session: &Session) -> Capabilities {
        let mut capabilities = self.capabilities();
        // DELIBERATE DIVERGENCE from the capability table in
        // specs/001-fleet-view-unification/data-model.md, which marks codex `stop` as an
        // unconditional yes and pid-gates opencode only. Reason: `stop` below already
        // returns Unsupported for a row with no pid, because with no pid there is no
        // process to signal. Advertising stop there would be a promise this backend
        // cannot keep, and a capability that is advertised and then fails at press time is
        // worse than one advertised as unsupported. Keep the gate; the table is the
        // coarser statement.
        //
        // A daemon-hosted row carries no pid BY DESIGN (the fd holder is the daemon, whose pid
        // must never be signalled), and it is stopped with `turn/interrupt` over the socket
        // instead. Gating on the pid alone would gray Ctrl+C out on exactly the rows that are
        // interruptible.
        //
        // Asking the router keeps the advertisement and the action in lockstep, which is what
        // the paragraph above is really about: every route that ends in Unsupported is a
        // promise this backend cannot keep.
        capabilities.stop &= !matches!(stop_route(session), StopRoute::Unsupported);
        capabilities
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        let threads = self.query_threads()?;
        // ONE process sweep per tick, giving both the open-rollout map and the set of threads a
        // client is sitting in (the liveness signal a daemon-held fd cannot provide).
        let scan = status::scan_codex_processes();
        // PR refs come out of the rollout transcripts (the registry has no PR column), and
        // ONE byte budget is shared across the tick so a first listing cannot read every
        // rollout on the box at once (1.8 GB here). Order decides which rows badge first:
        // live threads before archived ones, newest before oldest. Archived rollouts are 1.3
        // GB of that 1.8 and are hidden from the default view, so scanning them first would
        // delay the badge on every row the user can actually see.
        let mut budget = pr_scan::SCAN_BUDGET_BYTES;
        let mut pr_refs: Vec<Vec<crate::backend::PrRef>> = vec![Vec::new(); threads.len()];
        let mut scan_order: Vec<usize> = (0..threads.len()).collect();
        scan_order.sort_unstable_by_key(|&idx| {
            let thread = &threads[idx];
            (thread.archived, std::cmp::Reverse(thread.updated_at_ms))
        });
        for idx in scan_order {
            pr_refs[idx] = self
                .pr_scan
                .refs_for(&threads[idx].rollout_path, &mut budget);
        }
        let mut sessions = Vec::with_capacity(threads.len());
        for (idx, thread) in threads.into_iter().enumerate() {
            // The resolver canonicalizes once (cached) and returns the owning pid from the
            // same open map, so no per-tick canonicalize is needed here.
            let attached = scan.attached_threads.contains(&thread.id);
            let (status, pid, daemon_hosted) =
                self.resolver
                    .resolve_scanned(&thread.rollout_path, &scan, attached);
            let companion = thread.source.is_companion();
            let origin = if matches!(thread.source, source::Source::Exec) {
                SessionOrigin::Exec
            } else {
                SessionOrigin::Interactive
            };
            sessions.push(Session {
                backend: BackendKind::Codex,
                id: thread.id,
                short_id: None,
                origin,
                title: thread.title,
                cwd: thread.cwd,
                git_branch: thread.git_branch,
                status,
                created_at_ms: thread.created_at_ms,
                updated_at_ms: thread.updated_at_ms,
                hidden: thread.archived,
                companion,
                summary: thread.preview,
                pid,
                rollout_path: Some(thread.rollout_path),
                pr_refs: std::mem::take(&mut pr_refs[idx]),
                daemon_hosted,
            });
        }
        Ok(sessions)
    }

    fn tail(&self, session: &Session, max_events: usize) -> Result<Vec<crate::TailEvent>> {
        let Some(path) = session
            .rollout_path
            .as_deref()
            .filter(|path| path.is_file())
        else {
            return Ok(Vec::new());
        };
        let mut events = rollout::read_transcript_tail(path)?;
        if events.len() > max_events {
            events = events.split_off(events.len() - max_events);
        }
        Ok(events)
    }

    fn turn_activity(&self, session: &Session, window: std::time::Duration) -> Result<Vec<i64>> {
        let Some(path) = session
            .rollout_path
            .as_deref()
            .filter(|path| path.is_file())
        else {
            return Ok(Vec::new());
        };
        let mut timestamps = rollout::read_turn_activity(path, window)?;
        let Ok(db_path) = registry::find_state_db(&self.codex_home) else {
            return Ok(timestamps);
        };
        let Ok(registry) = Registry::open(&db_path) else {
            return Ok(timestamps);
        };
        let Ok(threads) = registry.threads() else {
            return Ok(timestamps);
        };

        let mut reachable = HashSet::from([session.id.clone()]);
        loop {
            let mut found_descendant = false;
            for thread in &threads {
                if reachable.contains(&thread.id)
                    || !thread
                        .parent_thread_id
                        .as_ref()
                        .is_some_and(|parent| reachable.contains(parent))
                {
                    continue;
                }
                reachable.insert(thread.id.clone());
                found_descendant = true;
                if let Ok(child_activity) =
                    rollout::read_turn_activity(&thread.rollout_path, window)
                {
                    timestamps.extend(child_activity);
                }
            }
            if !found_descendant {
                break;
            }
        }
        timestamps.sort_unstable();
        Ok(timestamps)
    }

    fn available_models(&self) -> Vec<String> {
        // default first, then the CLI catalog, falling back to the registry's used models.
        self.models_cache
            .get_or_init(|| {
                let mut discovered = codex_catalog_via_cli();
                if discovered.is_empty() {
                    discovered = codex_models_via_registry(&self.codex_home);
                }
                let mut models = vec![self.kind().default_model().to_string()];
                models.extend(discovered);
                crate::backend::dedup_preserve(models)
            })
            .clone()
    }

    fn spawn(
        &self,
        dir: &Path,
        task: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<SpawnResult> {
        if crate::platform::current_platform() != Platform::Linux {
            return Err(crate::error::Error::Unsupported(self.kind().name()));
        }
        // Prefer the shared daemon: a thread it hosts is the only kind that can be joined
        // later, and it may be started here (never stopped, never restarted). Skip the probe
        // entirely when exec is forced, so an opt-in never starts a daemon it will not use.
        let exec_opt_in = exec_spawn_opt_in();
        let daemon = (!exec_opt_in).then(app_server::ensure_daemon);
        // Keep the concrete reason: it is the whole content of the refusal below.
        let why = daemon
            .as_ref()
            .and_then(|d| d.as_ref().err().cloned())
            .unwrap_or_else(|| "no codex app-server daemon is listening".to_string());
        let daemon = daemon.and_then(std::result::Result::ok);
        match spawn_route(daemon.as_ref(), exec_opt_in) {
            SpawnRoute::Daemon(daemon) => {
                return match app_server::try_spawn_thread(daemon, dir, task, model, effort) {
                    // The daemon owns the thread, so its pid must never be handed back as a
                    // killable one. The exact thread id is safe to return and lets the TUI
                    // select the new row without guessing from cwd and creation time.
                    app_server::SpawnAttempt::Started(thread_id) => Ok(SpawnResult {
                        pid: None,
                        session_id: Some(thread_id),
                    }),
                    // Every other outcome is a hard failure carrying the daemon's own error.
                    // There is deliberately no exec fallback: it would either double-run the
                    // task (the thread already exists after `thread/start`) or silently hand
                    // back an unjoinable session, and both were real defects.
                    attempt => Err(crate::error::Error::Command(format!(
                        "codex app-server spawn failed: {}",
                        spawn_failure_reason(&attempt)
                    ))),
                };
            }
            SpawnRoute::Refuse => {
                return Err(crate::error::Error::Command(no_daemon_spawn_refusal(&why)));
            }
            SpawnRoute::Exec => {}
        }
        let cmd = codex_spawn_command(dir, task, model);
        let log_path = crate::default_codex_home()
            .join("bg-logs")
            .join(format!("{}.log", crate::spawn::now_ms()));
        let pid = crate::spawn::spawn_detached(cmd, &log_path)?;
        Ok(SpawnResult {
            pid: Some(pid),
            session_id: None,
        })
    }

    fn hide(&self, id: &str) -> Result<()> {
        cli::archive(id)
    }

    fn unhide(&self, id: &str) -> Result<()> {
        cli::unarchive(id)
    }

    fn stop(&self, session: &Session) -> Result<()> {
        if crate::platform::current_platform() != Platform::Linux {
            return Err(crate::error::Error::Unsupported(self.kind().name()));
        }
        match stop_route(session) {
            StopRoute::Interrupt => {
                // Probe only, never start: if no daemon answers, this thread is not hosted by
                // one any more, so there is no turn left to interrupt.
                let daemon = app_server::probe_daemon().ok_or_else(|| {
                    crate::error::Error::Command(
                        "no codex app-server daemon is listening to interrupt".into(),
                    )
                })?;
                app_server::interrupt_thread(&daemon, &session.id)
            }
            StopRoute::Signal(pid) => crate::spawn::terminate(pid, "codex"),
            StopRoute::Unsupported => Err(crate::error::Error::Unsupported(self.kind().name())),
        }
    }

    fn remove(&self, session: &Session) -> Result<()> {
        cli::archive(&session.id)
    }

    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        cli::rename(&session.id, name)
    }

    fn attach_command(
        &self,
        session: &Session,
    ) -> std::result::Result<std::process::Command, AttachRefusal> {
        let platform = crate::platform::current_platform();
        // Probe only, never start, and only for a row that claims a daemon host: the probe is
        // a shell-out and this runs on the attach keypress, so a Local or Refuse row must not
        // pay for it.
        let daemon = (platform == Platform::Linux && session.daemon_hosted)
            .then(app_server::probe_daemon)
            .flatten();
        let mut cmd = match attach_route_for_platform(session, daemon.as_ref(), platform) {
            // A real live join: `thread/resume` inside the hosting process returns the history
            // and atomically subscribes to live updates.
            AttachRoute::Remote(endpoint) => cli::resume_remote_command(&endpoint, &session.id),
            AttachRoute::Local => cli::resume_command(&session.id),
            // Not an error the user must dismiss: the row IS running, it just cannot be
            // joined, so the caller falls back to watching it read-only.
            AttachRoute::Refuse(reason) if platform == Platform::Linux => {
                return Err(AttachRefusal::tailable(reason));
            }
            AttachRoute::Refuse(reason) => return Err(AttachRefusal::new(reason)),
        };
        // `codex resume` inherits the viewer's cwd and otherwise prompts "Choose working
        // directory" on attach. Pin it to the session's own cwd when that directory still
        // exists; leave it unset when the dir was deleted so the spawn cannot fail on it.
        if session.cwd.is_dir() {
            cmd.current_dir(&session.cwd);
        }
        Ok(cmd)
    }
}

/// PURE capability policy for Codex on each supported release platform.
///
/// Portable builds retain read-only enumeration, transcript activity, and CLI mutations that
/// do not depend on process ownership. They do not claim liveness, daemon hosting, safe spawn,
/// attach, stop, or approval state.
pub const fn capabilities_for_platform(platform: Platform) -> Capabilities {
    match platform {
        Platform::Linux => Capabilities {
            spawn: true,
            attach: true,
            rename: true,
            archive: true,
            delete: true,
            stop: true,
            needs_input: true,
            pr_refs: true,
            live_status: true,
        },
        Platform::Macos | Platform::Windows => Capabilities {
            spawn: false,
            attach: false,
            rename: true,
            archive: true,
            delete: true,
            stop: false,
            needs_input: false,
            pr_refs: true,
            live_status: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::codex_spawn_command;
    use crate::backend::args;
    use std::path::Path;

    #[test]
    fn codex_spawn_command_runs_unsandboxed() {
        // Regression: `--sandbox workspace-write` mounts `.git` read-only and disables
        // network, so a spawned session could not fetch, branch, worktree, or commit and
        // reported "Read-only file system" instead of doing the work. Viewer-spawned
        // sessions run with the sandbox off; see SPEC.md "Spawn sandbox posture".
        let cmd = codex_spawn_command(Path::new("/tmp"), "t", None);
        let a = args(&cmd);
        assert!(
            a.iter()
                .any(|x| x == "--dangerously-bypass-approvals-and-sandbox"),
            "bypass flag missing: {a:?}"
        );
        assert!(
            !a.iter().any(|x| x == "--sandbox" || x == "-s"),
            "a sandbox flag would re-impose the read-only .git mount: {a:?}"
        );
        // The flag must precede the positional prompt, or clap reads it as prompt text.
        let bypass = a
            .iter()
            .position(|x| x == "--dangerously-bypass-approvals-and-sandbox")
            .expect("bypass present");
        let prompt = a.iter().position(|x| x == "t").expect("prompt present");
        assert!(bypass < prompt, "bypass must precede the prompt: {a:?}");
    }

    #[test]
    fn codex_spawn_command_carries_model_flag() {
        // A non-default model becomes `-m <model>`, before the task arg.
        let cmd = codex_spawn_command(Path::new("/tmp"), "do a thing", Some("gpt-5.3-codex"));
        let a = args(&cmd);
        let i = a.iter().position(|x| x == "-m").expect("-m present");
        assert_eq!(a[i + 1], "gpt-5.3-codex");
        assert_eq!(a.last().map(String::as_str), Some("do a thing"));

        // None (the "default" model) adds no -m flag.
        let cmd = codex_spawn_command(Path::new("/tmp"), "t", None);
        assert!(!args(&cmd).iter().any(|x| x == "-m"));
    }
}
