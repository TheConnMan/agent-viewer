#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Codex,
    Claude,
    Opencode,
}

impl BackendKind {
    /// "codex" | "claude" | "opencode"
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Codex => "codex",
            BackendKind::Claude => "claude",
            BackendKind::Opencode => "opencode",
        }
    }
    /// "[cx]" | "[cl]" | "[oc]"  (row prefix)
    pub fn tag(self) -> &'static str {
        match self {
            BackendKind::Codex => "[cx]",
            BackendKind::Claude => "[cl]",
            BackendKind::Opencode => "[oc]",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub spawn: bool,
    pub hide: bool,
    pub attach: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    Errored,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub backend: BackendKind,
    pub id: String,
    pub title: String,
    pub cwd: std::path::PathBuf,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub status: Status,
    pub hidden: bool,
    pub source_label: String,
}

pub trait Backend {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> Capabilities;
    /// &mut self: the codex impl caches per-rollout status by (mtime, len).
    /// Sessions are returned recency-sorted (updated_at_ms DESC).
    fn list(&mut self) -> crate::error::Result<Vec<Session>>;
    fn spawn(&self, dir: &std::path::Path, task: &str) -> crate::error::Result<()>;
    fn hide(&self, id: &str) -> crate::error::Result<()> {
        let _ = id;
        Err(crate::error::Error::Unsupported(self.kind().name()))
    }
    fn unhide(&self, id: &str) -> crate::error::Result<()> {
        let _ = id;
        Err(crate::error::Error::Unsupported(self.kind().name()))
    }
    /// None when attach is unsupported. Command is built, not run (TUI suspends into it).
    fn attach_command(&self, session: &Session) -> Option<std::process::Command>;
}

/// The fixed v1 roster: Codex (default_codex_home), Claude ("claude" on PATH),
/// Opencode (~/.local/share/opencode/opencode.db). No config surface.
pub fn all_backends() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(crate::codex::CodexBackend::new(crate::default_codex_home())),
        Box::new(crate::claude::ClaudeBackend::new()),
        Box::new(crate::opencode::OpencodeBackend::new()),
    ]
}
