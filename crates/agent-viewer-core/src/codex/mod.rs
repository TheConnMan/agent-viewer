pub mod cli;
pub mod registry;
pub mod rollout;
pub mod source;
pub mod status;

use crate::backend::{Backend, BackendKind, Capabilities, Session};
use crate::error::Result;
use registry::Registry;
use status::StatusResolver;
use std::path::Path;

// NOTE: the experimental `codex app-server` JSON-RPC daemon (`thread/subscribe`,
// `thread/list`, `command/exec/terminate`) is the eventual "clean" backend and the v2
// upgrade path. v1 reads the same SQLite + rollout files directly.

/// Sandbox args passed to `codex exec` for viewer-spawned sessions. Least-privileged
/// choice (verified working unattended on this box). If a workspace-write run ever fails
/// to complete unattended, switch to ["--dangerously-bypass-approvals-and-sandbox"].
const SANDBOX_ARGS: &[&str] = &["--sandbox", "workspace-write"];

pub struct CodexBackend {
    codex_home: std::path::PathBuf,
    registry: Option<Registry>,
    resolver: StatusResolver,
}

impl CodexBackend {
    pub fn new(codex_home: std::path::PathBuf) -> CodexBackend {
        CodexBackend {
            codex_home,
            registry: None,
            resolver: StatusResolver::new(),
        }
    }

    /// Query threads, reopening the DB once on failure (state_N rollover, transient error).
    fn query_threads(&mut self) -> Result<Vec<registry::Thread>> {
        if self.registry.is_none() {
            let db = registry::find_state_db(&self.codex_home)?;
            self.registry = Some(Registry::open(&db)?);
        }
        match self.registry.as_ref().unwrap().threads() {
            Ok(threads) => Ok(threads),
            Err(_) => {
                self.registry = None;
                let db = registry::find_state_db(&self.codex_home)?;
                let reg = Registry::open(&db)?;
                let threads = reg.threads()?;
                self.registry = Some(reg);
                Ok(threads)
            }
        }
    }
}

impl Backend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            spawn: true,
            hide: true,
            attach: true,
            stop: true,
            remove: true,
            rename: true,
        }
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        let threads = self.query_threads()?;
        let open = status::open_rollout_paths();
        let mut sessions = Vec::with_capacity(threads.len());
        for thread in threads {
            // The resolver canonicalizes once (cached) and returns the owning pid from the
            // same open map, so no per-tick canonicalize is needed here.
            let (status, pid) = self.resolver.resolve(&thread.rollout_path, &open);
            let companion = thread.source.is_companion();
            let source_label = thread.source.label().to_string();
            sessions.push(Session {
                backend: BackendKind::Codex,
                id: thread.id,
                short_id: None,
                title: thread.title,
                cwd: thread.cwd,
                created_at_ms: thread.created_at_ms,
                updated_at_ms: thread.updated_at_ms,
                status,
                hidden: thread.archived,
                source_label,
                summary: thread.preview,
                companion,
                pid,
                rollout_path: Some(thread.rollout_path),
                pr_refs: Vec::new(),
            });
        }
        Ok(sessions)
    }

    fn spawn(&self, dir: &Path, task: &str, model: Option<&str>) -> Result<Option<u32>> {
        let mut cmd = std::process::Command::new("codex");
        cmd.arg("exec").arg("--json").arg("-C").arg(dir);
        cmd.args(SANDBOX_ARGS);
        if let Some(model) = model {
            cmd.arg("-m").arg(model);
        }
        cmd.arg(task);
        let log_path = crate::default_codex_home()
            .join("bg-logs")
            .join(format!("{}.log", crate::spawn::now_ms()));
        let pid = crate::spawn::spawn_detached(cmd, &log_path)?;
        Ok(Some(pid))
    }

    fn hide(&self, id: &str) -> Result<()> {
        cli::archive(id)
    }

    fn unhide(&self, id: &str) -> Result<()> {
        cli::unarchive(id)
    }

    fn stop(&self, session: &Session) -> Result<()> {
        match session.pid {
            Some(pid) => crate::spawn::terminate(pid, "codex"),
            None => Err(crate::error::Error::Unsupported(self.kind().name())),
        }
    }

    fn remove(&self, id: &str) -> Result<()> {
        cli::archive(id)
    }

    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        cli::rename(&session.id, name)
    }

    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        let mut cmd = cli::resume_command(&session.id);
        // `codex resume` inherits the viewer's cwd and otherwise prompts "Choose working
        // directory" on attach. Pin it to the session's own cwd when that directory still
        // exists; leave it unset when the dir was deleted so the spawn cannot fail on it.
        if session.cwd.is_dir() {
            cmd.current_dir(&session.cwd);
        }
        Some(cmd)
    }
}
