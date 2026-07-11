use crate::backend::{Backend, BackendKind, Capabilities, Session};
use crate::error::Result;

pub struct ClaudeBackend {
    binary: String,
}

impl ClaudeBackend {
    pub fn new() -> ClaudeBackend {
        todo!()
    }
    pub fn with_binary(binary: &str) -> ClaudeBackend {
        let _ = binary;
        todo!()
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for ClaudeBackend {
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

/// PURE parser, unit-tested against the fixture. Input: stdout of `claude agents --json`
/// — a JSON array of objects.
/// Mapping: id = sessionId (attach takes it); title = name; cwd = cwd;
/// created_at_ms = updated_at_ms = startedAt; hidden = false; source_label = kind;
/// state: "working" -> Running, "done" -> Done, "blocked" -> Errored (attention),
///        anything else -> Errored. Entries missing sessionId/cwd/name are SKIPPED.
/// Non-array top level -> Err(Json).
pub fn parse_agents_json(stdout: &str) -> Result<Vec<Session>> {
    let _ = stdout;
    todo!()
}
