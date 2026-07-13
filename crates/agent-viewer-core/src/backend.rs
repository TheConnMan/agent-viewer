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
    /// Reply into a blocked/live session (codex approval keystroke T, claude text T,
    /// opencode F).
    pub reply: bool,
}

impl Capabilities {
    /// All-false capabilities: the fallback for an absent backend and a minimal test stub.
    pub const fn none() -> Capabilities {
        Capabilities {
            spawn: false,
            hide: false,
            attach: false,
            stop: false,
            remove: false,
            rename: false,
            reply: false,
        }
    }
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

/// A pull request associated with a session, from a claude jobs state.json child
/// (kind=="pr"). `id` is the display ref (e.g. "315"); `href` is the full GitHub URL
/// when present, used to resolve owner/repo/number for a live status lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    pub id: String,
    pub href: Option<String>,
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
    pub pr_refs: Vec<PrRef>,
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
    /// `model` is the optional per-spawn model (claude `--model`, codex/opencode `-m`);
    /// None uses the backend's own default.
    fn spawn(
        &self,
        dir: &std::path::Path,
        task: &str,
        model: Option<&str>,
    ) -> crate::error::Result<Option<u32>>;
    /// Candidate models for the composer's model picker, DEFAULT-FIRST and deduped.
    /// Discovery is best-effort and cached; a failing probe degrades to just the default.
    fn available_models(&self) -> Vec<String> {
        vec!["default".to_string()]
    }
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
    fn remove(&self, session: &Session) -> crate::error::Result<()> {
        let _ = session;
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

/// Remove duplicates while preserving first-seen order (case-sensitive exact match).
/// Used by every backend's `available_models` to fold the leading default in with the
/// discovered slugs without reordering them.
pub(crate) fn dedup_preserve(v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(v.len());
    for item in v {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
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

/// Test-only: a command's args as owned Strings (shared by the per-backend
/// spawn-command tests in `claude` and `codex`).
#[cfg(test)]
pub(crate) fn args(cmd: &std::process::Command) -> Vec<String> {
    cmd.get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_preserve_keeps_first_seen_order() {
        let got = dedup_preserve(vec![
            "default".to_string(),
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
            "b".to_string(),
        ]);
        assert_eq!(got, vec!["default", "a", "b", "c"]);
    }

    #[test]
    fn dedup_preserve_is_case_sensitive() {
        let got = dedup_preserve(vec!["A".to_string(), "a".to_string(), "A".to_string()]);
        assert_eq!(got, vec!["A", "a"]);
    }

    #[test]
    fn trait_default_available_models_is_just_default() {
        // A minimal backend that overrides nothing gets the trait fallback: `["default"]`.
        struct Dummy;
        impl Backend for Dummy {
            fn kind(&self) -> BackendKind {
                BackendKind::Codex
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities::none()
            }
            fn list(&mut self) -> crate::error::Result<Vec<Session>> {
                Ok(Vec::new())
            }
            fn spawn(
                &self,
                _dir: &std::path::Path,
                _task: &str,
                _model: Option<&str>,
            ) -> crate::error::Result<Option<u32>> {
                Ok(None)
            }
            fn attach_command(&self, _session: &Session) -> Option<std::process::Command> {
                None
            }
        }
        assert_eq!(Dummy.available_models(), vec!["default".to_string()]);
    }
}
