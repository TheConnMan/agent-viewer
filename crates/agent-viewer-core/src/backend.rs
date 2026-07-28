#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    Codex,
    Claude,
    Opencode,
}

/// Identities produced by a successful backend spawn.
///
/// `pid` belongs only to a direct viewer child that may be recorded for pinning and stop.
/// `session_id` is the backend's exact identity when the spawn protocol returns one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnResult {
    pub pid: Option<u32>,
    pub session_id: Option<String>,
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
    /// The model label a spawn uses when the user has picked nothing: the leading entry of
    /// `available_models`, and the composer's placeholder while discovery is still running.
    /// Codex and opencode take "default" (their CLIs resolve it themselves); claude has no
    /// such passthrough, so its label is a real model id.
    pub fn default_model(self) -> &'static str {
        match self {
            BackendKind::Codex | BackendKind::Opencode => "default",
            BackendKind::Claude => "opus[1m]",
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

/// How a session was started, independent of backend — used to decide attachability and
/// short-id expectations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionOrigin {
    /// Daemon or job managed, carries a short id, and is attachable.
    Background,
    /// A human's own terminal session with no short id.
    Interactive,
    /// A one-shot, noninteractive run.
    Exec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub spawn: bool,
    pub attach: bool,
    pub rename: bool,
    pub archive: bool,
    pub delete: bool,
    pub stop: bool,
    pub needs_input: bool,
    pub pr_refs: bool,
    pub live_status: bool,
}

impl Capabilities {
    /// All-false capabilities: the fallback for an absent backend and a minimal test stub.
    pub const fn none() -> Capabilities {
        Capabilities {
            spawn: false,
            attach: false,
            rename: false,
            archive: false,
            delete: false,
            stop: false,
            needs_input: false,
            pr_refs: false,
            live_status: false,
        }
    }
}

/// PURE capability ceiling for the OpenCode implementation available on each platform.
///
/// The Linux backend may narrow this at runtime when its authenticated server is unavailable.
/// Portable builds expose only the read only SQLite and CLI compatibility actions.
pub const fn opencode_capabilities_for_platform(
    platform: crate::platform::Platform,
) -> Capabilities {
    match platform {
        crate::platform::Platform::Linux => Capabilities {
            spawn: true,
            attach: true,
            rename: true,
            archive: true,
            delete: true,
            stop: true,
            needs_input: true,
            pr_refs: false,
            live_status: true,
        },
        crate::platform::Platform::Macos | crate::platform::Platform::Windows => Capabilities {
            spawn: true,
            attach: true,
            rename: false,
            archive: false,
            delete: false,
            stop: false,
            needs_input: false,
            pr_refs: false,
            live_status: false,
        },
    }
}

/// Six-state model. `Working`/`NeedsInput`/`Idle` describe a live session; `Done`/`Error`
/// describe a finished one. `Unknown` is the deliberate escape hatch for "the backend cannot
/// say": a resolver that cannot determine status MUST return `Unknown` rather than
/// fabricating `Idle` — a false idle reads as a live session with nothing happening, which is
/// worse than an honest "we don't know".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Status {
    Working,
    NeedsInput { reason: Option<String> },
    Idle,
    Done,
    Error,
    Unknown,
}

impl Status {
    pub fn needs_input() -> Status {
        Status::NeedsInput { reason: None }
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, Status::Done | Status::Error)
    }
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
    /// Claude's short agents-JSON `id` (the jobs-dir key). Only Claude sessions carry one;
    /// every other backend leaves it `None`. Used for the live agents-view attach and the
    /// jobs `state.json` path.
    pub short_id: Option<String>,
    pub origin: SessionOrigin,
    pub title: String,
    pub cwd: std::path::PathBuf,
    pub git_branch: Option<String>,
    pub status: Status,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub hidden: bool,
    /// True for a session that is a secondary view of one already shown elsewhere in the
    /// list (a companion row for the same underlying session) and so hidden from the default
    /// view. The overlay clears this for viewer-spawned (pinned) sessions so they always
    /// show, even if their backend would otherwise mark them a companion.
    pub companion: bool,
    /// One-line dim summary for the row. codex: threads.preview; claude: state.json needs
    /// (blocked) else detail; opencode: "".
    pub summary: String,
    /// Live PID when known — codex: the process holding the rollout fd; claude: agents-json
    /// "pid" (present on working entries); opencode: filled by the overlay from the viewer
    /// spawn record.
    pub pid: Option<u32>,
    /// Some for codex — the rollout JSONL, OR the claude session JSONL (state.json
    /// linkScanPath) for peek. None for opencode.
    pub rollout_path: Option<std::path::PathBuf>,
    /// Associated PR references (claude jobs `state.json` children where kind=="pr");
    /// rendered as a right-aligned badge. Empty for codex/opencode.
    pub pr_refs: Vec<PrRef>,
    /// True when this session lives inside a shared backend runtime whose process hosts
    /// multiple sessions. Such a row carries no `pid` because that process belongs to every
    /// session it hosts. Codex joins and stops these rows through its app server. OpenCode
    /// joins and stops them through its server API.
    pub daemon_hosted: bool,
}

/// A push notification from a backend that supports `subscribe`: either one session's status
/// changed, or the backend's whole listing should be treated as stale and re-fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusEvent {
    Changed {
        backend: BackendKind,
        id: String,
        status: Status,
    },
    Invalidated {
        backend: BackendKind,
    },
}

/// The callback a subscriber hands to a backend's `subscribe`. It may be called from
/// whatever thread the backend's push mechanism runs on (not necessarily the UI thread), so
/// it must be `Send + Sync` and cheap.
pub type StatusSink = std::sync::Arc<dyn Fn(StatusEvent) + Send + Sync>;

/// A live push subscription. Dropping it unsubscribes: the backend's `stop` closure runs
/// exactly once, on drop, so a subscription can never outlive its owner and leak a
/// background thread or listener.
pub struct Subscription {
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl Subscription {
    pub fn inactive() -> Subscription {
        Subscription { stop: None }
    }

    pub fn new(stop: impl FnOnce() + Send + 'static) -> Subscription {
        Subscription {
            stop: Some(Box::new(stop)),
        }
    }

    pub fn is_active(&self) -> bool {
        self.stop.is_some()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

/// `Send` so the TUI can move the listing backends onto a dedicated refresh thread
/// (each impl's state — rusqlite Connection, caches, PathBuf — is already Send).
pub trait Backend: Send {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> Capabilities;
    /// Per-row capability override. Most rows just get the backend-wide `capabilities()`,
    /// but some actions are only valid for particular rows (e.g. a row lacking the short id
    /// a destructive action needs). This exists because a capability advertised and then
    /// failing at press time is worse than one advertised unsupported up front: the UI
    /// trusts this to decide what to gray out, not what to attempt and catch.
    fn capabilities_for(&self, session: &Session) -> Capabilities {
        let _ = session;
        self.capabilities()
    }
    /// &mut self: the codex impl caches per-rollout status by (mtime, len).
    /// Sessions are returned recency-sorted (updated_at_ms DESC).
    fn list(&mut self) -> crate::error::Result<Vec<Session>>;
    /// Epoch millisecond timestamps of turn events for `session` within the last
    /// `window`, oldest first. Empty when the backend cannot say.
    fn turn_activity(
        &self,
        session: &Session,
        window: std::time::Duration,
    ) -> crate::error::Result<Vec<i64>> {
        let _ = (session, window);
        Ok(Vec::new())
    }
    /// Returns the direct child PID when the viewer forked it and the exact backend session
    /// identity when the spawn protocol provides one.
    /// `model` is the optional per-spawn model (claude `--model`, codex/opencode `-m`);
    /// None uses the backend's own default.
    fn spawn(
        &self,
        dir: &std::path::Path,
        task: &str,
        model: Option<&str>,
    ) -> crate::error::Result<SpawnResult>;
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
    /// Builds (never runs) the attach command; the TUI suspends into it. `Err` carries an
    /// `AttachRefusal` with a human-readable reason (no live pid, unsupported backend, ...)
    /// so the caller can show it verbatim in a footer notice instead of a generic failure.
    fn attach_command(
        &self,
        session: &Session,
    ) -> std::result::Result<std::process::Command, crate::error::AttachRefusal>;
    /// Opt-in push notifications for status changes. The default is a no-op (an inactive
    /// `Subscription`) so backends can adopt push incrementally: every backend already works
    /// correctly on the refresh worker's poll loop, so overriding this is a pure
    /// optimization, never a requirement, and the poll loop stays as the backstop regardless.
    fn subscribe(&self, sink: StatusSink) -> crate::error::Result<Subscription> {
        let _ = sink;
        Ok(Subscription::inactive())
    }
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

/// The fixed v1 roster. OpenCode uses its server first and falls back to its read only
/// SQLite registry when no managed server is healthy. No config surface.
pub fn all_backends() -> Vec<Box<dyn Backend>> {
    all_backends_with_opencode(crate::opencode::OpencodeRuntime::new())
}

/// The fixed v1 roster with a caller supplied OpenCode runtime. Callers with multiple
/// backend sets use this factory so listing and actions observe the same server state.
pub fn all_backends_with_opencode(
    runtime: crate::opencode::OpencodeRuntime,
) -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(crate::codex::CodexBackend::new(crate::default_codex_home())),
        Box::new(crate::claude::ClaudeBackend::new()),
        Box::new(crate::opencode::OpencodeBackend::with_runtime(runtime)),
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
            ) -> crate::error::Result<SpawnResult> {
                Ok(SpawnResult::default())
            }
            fn attach_command(
                &self,
                _session: &Session,
            ) -> std::result::Result<std::process::Command, crate::error::AttachRefusal>
            {
                Err(crate::error::AttachRefusal::new(
                    "dummy sessions cannot be attached",
                ))
            }
        }
        assert_eq!(Dummy.available_models(), vec!["default".to_string()]);
    }
}
