use crate::backend::{Backend, BackendKind, Capabilities, Session, Status};
use crate::error::Result;

pub struct OpencodeBackend {
    db_path: std::path::PathBuf,
}

impl OpencodeBackend {
    /// Default path: $HOME/.local/share/opencode/opencode.db
    pub fn new() -> OpencodeBackend {
        todo!()
    }
    pub fn with_db(db_path: std::path::PathBuf) -> OpencodeBackend {
        let _ = db_path;
        todo!()
    }
}

impl Default for OpencodeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for OpencodeBackend {
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
    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        let _ = session;
        todo!()
    }
}

/// PURE status heuristic, unit-tested (the process check is injected):
/// Running iff live_opencode_proc && (now_ms - updated_at_ms) <= 60_000; else Done.
/// Never returns Errored.
pub fn opencode_status(live_opencode_proc: bool, updated_at_ms: i64, now_ms: i64) -> Status {
    let _ = (live_opencode_proc, updated_at_ms, now_ms);
    todo!()
}
