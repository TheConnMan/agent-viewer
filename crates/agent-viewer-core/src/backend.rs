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
    /// "[cx]" | "[cc]" | "[oc]"  (row + composer prefix)
    pub fn tag(self) -> &'static str {
        match self {
            BackendKind::Codex => "[cx]",
            BackendKind::Claude => "[cc]",
            BackendKind::Opencode => "[oc]",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub spawn: bool,
    pub hide: bool,
    pub attach: bool,
    /// SIGTERM the live process (codex T, claude F per I-4, opencode T).
    pub stop: bool,
    /// Second-stage Ctrl+X hard-remove (codex T archive, claude F, opencode T delete).
    pub remove: bool,
    /// Rename in the backend's own store (codex T, claude T UDS best-effort, opencode T).
    pub rename: bool,
}

/// Six-state model (v2). `Working`/`Failed` are v1's `Running`/`Errored` renamed;
/// `NeedsInput`, `Idle`, `Stopped` are new. `Hash` is used to key sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Working,
    NeedsInput,
    Idle,
    Done,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub backend: BackendKind,
    pub id: String,
    /// Claude's short agents-JSON "id" (the jobs-dir key). Only claude sessions carry
    /// one; every other backend leaves it None. Used for the live agents-view attach
    /// and the jobs `state.json` path.
    pub short_id: Option<String>,
    pub title: String,
    pub cwd: std::path::PathBuf,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub status: Status,
    pub hidden: bool,
    pub source_label: String,
    /// One-line dim summary for the row. codex: threads.preview; claude:
    /// state.json needs (blocked) else detail; opencode: "".
    pub summary: String,
    /// True for rows the default view hides: codex source Exec|Subagent(_),
    /// opencode parent_id NOT NULL. Claude rows are never companions.
    /// The overlay clears this for viewer-spawned (pinned) sessions.
    pub companion: bool,
    /// Live PID when known — codex: the process holding the rollout fd;
    /// claude: agents-json "pid" (present on working entries); opencode: filled
    /// by the overlay from the viewer spawn record.
    pub pid: Option<u32>,
    /// Some for codex — the rollout JSONL, OR the claude session JSONL
    /// (state.json linkScanPath) for peek. None for opencode.
    pub rollout_path: Option<std::path::PathBuf>,
    /// Associated PR references (claude jobs `state.json` children where kind=="pr");
    /// rendered as a right-aligned badge. Empty for codex/opencode.
    pub pr_refs: Vec<String>,
}

/// `Send` so the TUI can move the listing backends onto a dedicated refresh thread
/// (each impl's state — rusqlite Connection, caches, PathBuf — is already Send).
pub trait Backend: Send {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> Capabilities;
    /// &mut self: the codex impl caches per-rollout status by (mtime, len).
    /// Sessions are returned recency-sorted (updated_at_ms DESC).
    fn list(&mut self) -> crate::error::Result<Vec<Session>>;
    /// Returns the direct child PID when the viewer forked it (codex, opencode);
    /// None when the tool self-detaches its real worker (claude --bg).
    /// The TUI records Some(pid) in the viewer DB (spawn pinning + stop).
    fn spawn(&self, dir: &std::path::Path, task: &str) -> crate::error::Result<Option<u32>>;
    fn hide(&self, id: &str) -> crate::error::Result<()> {
        let _ = id;
        Err(crate::error::Error::Unsupported(self.kind().name()))
    }
    fn unhide(&self, id: &str) -> crate::error::Result<()> {
        let _ = id;
        Err(crate::error::Error::Unsupported(self.kind().name()))
    }
    /// SIGTERM the live session process (session.pid required; runtime-gated).
    fn stop(&self, session: &Session) -> crate::error::Result<()> {
        let _ = session;
        Err(crate::error::Error::Unsupported(self.kind().name()))
    }
    /// Second-stage Ctrl+X hard-remove. Default Unsupported.
    fn remove(&self, id: &str) -> crate::error::Result<()> {
        let _ = id;
        Err(crate::error::Error::Unsupported(self.kind().name()))
    }
    /// Rename in the backend's own store (never a raw DB write). Default Unsupported.
    fn rename(&self, session: &Session, name: &str) -> crate::error::Result<()> {
        let _ = (session, name);
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
