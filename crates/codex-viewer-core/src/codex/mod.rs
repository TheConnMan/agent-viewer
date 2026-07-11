pub mod cli;
pub mod registry;
pub mod rollout;
pub mod source;
pub mod status;

use crate::backend::{Backend, BackendKind, Capabilities, Session};
use crate::error::Result;

// NOTE: the experimental `codex app-server` JSON-RPC daemon (`thread/subscribe`,
// `thread/list`, `command/exec/terminate`) is the eventual "clean" backend and the v2
// upgrade path. v1 reads the same SQLite + rollout files directly.

pub struct CodexBackend {
    codex_home: std::path::PathBuf,
    registry: Option<registry::Registry>,
    resolver: status::StatusResolver,
}

impl CodexBackend {
    pub fn new(codex_home: std::path::PathBuf) -> CodexBackend {
        let _ = codex_home;
        todo!()
    }
}

impl Backend for CodexBackend {
    fn kind(&self) -> BackendKind {
        todo!()
    }
    fn capabilities(&self) -> Capabilities {
        todo!()
    }
    fn list(&mut self) -> Result<Vec<Session>> {
        todo!()
    }
    fn spawn(&self, dir: &std::path::Path, task: &str) -> Result<()> {
        let _ = (dir, task);
        todo!()
    }
    fn hide(&self, id: &str) -> Result<()> {
        let _ = id;
        todo!()
    }
    fn unhide(&self, id: &str) -> Result<()> {
        let _ = id;
        todo!()
    }
    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        let _ = session;
        todo!()
    }
}
