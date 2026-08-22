#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BackendKind {
    Codex,
    Claude,
    Grok,
}

/// A nonsecret namespace for one concrete backend listing source.
///
/// The backend identity is kept separately from the opaque instance key so SQLite can
/// validate both before returning a shared snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListingCacheScope {
    backend: BackendKind,
    key: String,
}

impl ListingCacheScope {
    pub fn new(
        backend: BackendKind,
        key: impl Into<String>,
    ) -> crate::error::Result<ListingCacheScope> {
        let key = key.into();
        if key.is_empty() {
            return Err(crate::error::Error::Command(
                "listing cache scope cannot be empty".to_string(),
            ));
        }
        Ok(ListingCacheScope { backend, key })
    }

    pub const fn backend(&self) -> BackendKind {
        self.backend
    }

    pub fn as_str(&self) -> &str {
        &self.key
    }
}

pub(crate) fn listing_scope_key(parts: &[String]) -> String {
    serde_json::to_string(parts).expect("serializing strings cannot fail")
}

pub(crate) fn listing_scope_path(path: &std::path::Path) -> String {
    let effective = std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    });
    scope_os_identity(effective.as_os_str())
}

#[cfg(unix)]
fn scope_os_identity(value: &std::ffi::OsStr) -> String {
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt as _;

    let bytes = value.as_bytes();
    let mut identity = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(identity, "{byte:02x}").expect("writing to a String cannot fail");
    }
    identity
}

#[cfg(windows)]
fn scope_os_identity(value: &std::ffi::OsStr) -> String {
    use std::fmt::Write as _;
    use std::os::windows::ffi::OsStrExt as _;

    let mut identity = String::new();
    for unit in value.encode_wide() {
        write!(identity, "{unit:04x}").expect("writing to a String cannot fail");
    }
    identity
}

pub(crate) fn listing_scope_executable(binary: &str) -> String {
    let path = std::path::Path::new(binary);
    if path.is_absolute() || path.components().count() > 1 {
        return listing_scope_path(path);
    }
    if let Some(candidate) = std::env::var_os("PATH")
        .as_deref()
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(path))
        .find(|candidate| candidate.is_file())
    {
        return listing_scope_path(&candidate);
    }
    binary.to_string()
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
    /// "codex" | "claude" | "grok"
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Codex => "codex",
            BackendKind::Claude => "claude",
            BackendKind::Grok => "grok",
        }
    }
    /// The model label a spawn uses when the user has picked nothing: the leading entry of
    /// `available_models`, and the composer's placeholder while discovery is still running.
    /// Codex takes "default" (its CLI resolves it itself); claude has no
    /// such passthrough, so its label is a real model id.
    pub fn default_model(self) -> &'static str {
        match self {
            BackendKind::Codex => "default",
            BackendKind::Claude => "opus[1m]",
            BackendKind::Grok => "default",
        }
    }
    /// "[cx]" | "[cc]"  (row + composer prefix)
    pub fn tag(self) -> &'static str {
        match self {
            BackendKind::Codex => "[cx]",
            BackendKind::Claude => "[cc]",
            BackendKind::Grok => "[gx]",
        }
    }
}

/// How a session was started, independent of backend — used to decide attachability and
/// short-id expectations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
        }
    }
}

/// Six-state model. `Working`/`NeedsInput`/`Idle` describe a live session; `Done`/`Error`
/// describe a finished one. `Unknown` is the deliberate escape hatch for "the backend cannot
/// say": a resolver that cannot determine status MUST return `Unknown` rather than
/// fabricating `Idle` — a false idle reads as a live session with nothing happening, which is
/// worse than an honest "we don't know".
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrRef {
    pub id: String,
    pub href: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// True when the backend's own source record says this row is a SUBAGENT thread - a
    /// child a parent process spawned - rather than a process's own primary session.
    ///
    /// Kept separate from `companion` on purpose. `companion` is PRESENTATIONAL and anything
    /// may set it: `mark_dead_dirs` flags every session whose cwd has been deleted, so a
    /// perfectly ordinary CLI session in a removed worktree becomes a companion. Routing
    /// decisions (whose process is that pid?) must read this field instead, or a deleted
    /// directory silently strips a real session of its stop action.
    pub subagent: bool,
    /// One-line dim summary for the row. codex: threads.preview; claude: state.json needs
    /// (blocked) else detail.
    pub summary: String,
    /// Live PID when known — codex: the process holding the rollout fd; claude: agents-json
    /// "pid" (present on working entries).
    pub pid: Option<u32>,
    /// Some for codex — the rollout JSONL, OR the claude session JSONL (state.json
    /// linkScanPath) for peek.
    pub rollout_path: Option<std::path::PathBuf>,
    /// Associated PR references, rendered as a right-aligned badge. Claude: jobs
    /// `state.json` children where kind=="pr". Codex and Grok: github PR links from
    /// successful PR-creation tool calls in their transcripts, since those stores
    /// record none.
    pub pr_refs: Vec<PrRef>,
    /// True when this session lives inside a shared backend runtime whose process hosts
    /// multiple sessions. Such a row carries no `pid` because that process belongs to every
    /// session it hosts. Codex joins and stops these rows through its app server.
    pub daemon_hosted: bool,
}

/// One entry of a session's recent transcript, oldest first.
///
/// Prose is display-sanitized (the caller wraps it); a tool event is the tool's own name
/// plus the one argument worth showing, already squashed onto a single line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailEvent {
    Agent(String),
    User(String),
    Tool { name: String, detail: String },
}

/// Keys the backends use for the one argument worth showing on a tool line, tried in
/// this order. Codex writes `cmd`, Claude writes `command`/`file_path`; the rest are the
/// next-best identifying argument when none of those is present.
const TOOL_DETAIL_KEYS: [&str; 9] = [
    "cmd",
    "command",
    "file_path",
    "filePath",
    "path",
    "pattern",
    "query",
    "skill",
    "description",
];

/// The one argument worth showing for a tool call, pulled out of its input object. A
/// `command` given as an argv array joins with spaces. Nothing recognized is an empty
/// detail, never an error: a tool line still carries its name.
pub(crate) fn tool_detail(input: &serde_json::Value) -> String {
    for key in TOOL_DETAIL_KEYS {
        let Some(value) = input.get(key) else {
            continue;
        };
        let text = match value {
            serde_json::Value::String(text) => squash(text),
            serde_json::Value::Array(items) => squash(
                &items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => continue,
        };
        let text = sanitize_display_text(&text);
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// Collapse every run of whitespace to one space, so a multi-line shell script renders as
/// one tool line instead of silently eating the rest of the pane.
pub(crate) fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drop C0/C1 controls, bidi overrides, and other format characters that can spoof a
/// list row or leak into the parent terminal.
///
/// The character set is the union of the TUI title sanitizer and Grok's C0/C1+bidi
/// filter. Matching characters are dropped, not mapped to space: existing title tests
/// pin concatenation of `\n`/`\r`. Grok's `is_terminal_safe` identity rejection is a
/// separate check and is not relaxed into this strip.
pub fn sanitize_display_text(text: &str) -> String {
    text.chars()
        .filter(|character| is_display_safe(*character))
        .collect()
}

fn is_display_safe(character: char) -> bool {
    !character.is_control()
        && !matches!(
            character,
            '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
}

/// `Send` so the TUI can move the listing backends onto a dedicated refresh thread
/// (each impl's state — rusqlite Connection, caches, PathBuf — is already Send).
pub trait Backend: Send {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> Capabilities;
    /// The concrete, nonsecret namespace whose raw display listings may be shared.
    /// `None` requires an authoritative list on every refresh.
    fn listing_scope(&self) -> Option<ListingCacheScope> {
        None
    }
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
    /// Sessions are returned by updated_at_ms ascending.
    fn list(&mut self) -> crate::error::Result<Vec<Session>>;
    /// Epoch millisecond timestamps of turn events for `session` and its descendant
    /// subtree within the last `window`, oldest first. Empty when the backend cannot say.
    fn turn_activity(
        &self,
        session: &Session,
        window: std::time::Duration,
    ) -> crate::error::Result<Vec<i64>> {
        let _ = (session, window);
        Ok(Vec::new())
    }
    /// The last `max_events` transcript events for `session`, oldest first, for the tail
    /// pane. Reads the backend's own store and NEVER starts a process: the pane retargets on
    /// every arrow key, so a spawn here would be one agent process per keystroke. Empty when
    /// the backend cannot say.
    fn tail(&self, session: &Session, max_events: usize) -> crate::error::Result<Vec<TailEvent>> {
        let _ = (session, max_events);
        Ok(Vec::new())
    }
    /// Returns the direct child PID when the viewer forked it and the exact backend session
    /// identity when the spawn protocol provides one.
    /// `model` is the optional per-spawn model (claude `--model`, codex `-m`);
    /// None uses the backend's own default.
    /// `effort` is the optional per-spawn reasoning effort; it is advisory and is ignored by
    /// backends without the concept.
    fn spawn(
        &self,
        dir: &std::path::Path,
        task: &str,
        model: Option<&str>,
        effort: Option<&str>,
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

/// The fixed v1 roster. No config surface.
#[cfg(target_os = "linux")]
pub fn all_backends() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(crate::codex::CodexBackend::new(crate::default_codex_home())),
        Box::new(crate::claude::ClaudeBackend::new()),
        Box::new(crate::grok::GrokBackend::new()),
    ]
}

/// The fixed v1 roster. No config surface.
#[cfg(not(target_os = "linux"))]
pub fn all_backends() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(crate::codex::CodexBackend::new(crate::default_codex_home())),
        Box::new(crate::claude::ClaudeBackend::new()),
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
    #[cfg(target_os = "linux")]
    fn linux_backend_roster_includes_grok() {
        let names: Vec<_> = all_backends()
            .into_iter()
            .map(|backend| backend.kind().name())
            .collect();

        assert_eq!(names, vec!["codex", "claude", "grok"]);
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn non_linux_backend_roster_excludes_grok() {
        let names: Vec<_> = all_backends()
            .into_iter()
            .map(|backend| backend.kind().name())
            .collect();

        assert_eq!(names, vec!["codex", "claude"]);
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
                _effort: Option<&str>,
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

    #[test]
    fn sanitize_display_text_drops_controls_bidi_and_format_chars() {
        assert_eq!(
            sanitize_display_text("safe\u{1b}[31m \u{202e}bidi\u{200b}zwsp"),
            "safe[31m bidizwsp"
        );
        assert_eq!(
            sanitize_display_text("line\ncarriage\rdelete\u{7f} c1\u{80}\u{9b}31m"),
            "linecarriagedelete c131m"
        );
        let clean = "Café 東京 🦀 Deploy";
        assert_eq!(sanitize_display_text(clean), clean);
        assert_eq!(
            sanitize_display_text("Grok leader peer is not owned by the current user"),
            "Grok leader peer is not owned by the current user"
        );
    }
}
