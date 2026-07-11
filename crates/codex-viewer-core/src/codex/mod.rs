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

/// Sandbox flag passed to `codex exec` for viewer-spawned sessions. Least-privileged
/// choice (verified working unattended on this box). If a workspace-write run ever fails
/// to complete unattended, switch to "--dangerously-bypass-approvals-and-sandbox".
const SANDBOX_FLAG: &str = "--sandbox workspace-write";

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
        }
    }

    fn list(&mut self) -> Result<Vec<Session>> {
        let threads = self.query_threads()?;
        let open = status::open_rollout_paths();
        let mut sessions = Vec::with_capacity(threads.len());
        for thread in threads {
            let status = self.resolver.resolve(&thread.rollout_path, &open);
            sessions.push(Session {
                backend: BackendKind::Codex,
                id: thread.id,
                title: thread.title,
                cwd: thread.cwd,
                created_at_ms: thread.created_at_ms,
                updated_at_ms: thread.updated_at_ms,
                status,
                hidden: thread.archived,
                source_label: thread.source.label().to_string(),
                rollout_path: Some(thread.rollout_path.clone()),
            });
        }
        Ok(sessions)
    }

    fn spawn(&self, dir: &Path, task: &str) -> Result<()> {
        let mut cmd = std::process::Command::new("codex");
        cmd.arg("exec").arg("--json").arg("-C").arg(dir);
        for flag in SANDBOX_FLAG.split_whitespace() {
            cmd.arg(flag);
        }
        cmd.arg(task);
        let log_path = crate::default_codex_home()
            .join("bg-logs")
            .join(format!("{}.log", crate::spawn::now_ms()));
        crate::spawn::spawn_detached(cmd, &log_path)?;
        Ok(())
    }

    fn hide(&self, id: &str) -> Result<()> {
        cli::archive(id)
    }

    fn unhide(&self, id: &str) -> Result<()> {
        cli::unarchive(id)
    }

    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        Some(cli::resume_command(&session.id))
    }
}
