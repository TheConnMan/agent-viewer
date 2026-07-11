use crate::backend::{Backend, BackendKind, Capabilities, Session};
use crate::error::Result;

pub struct ClaudeBackend {
    binary: String,
}

impl ClaudeBackend {
    pub fn new() -> ClaudeBackend {
        ClaudeBackend {
            binary: "claude".to_string(),
        }
    }
    pub fn with_binary(binary: &str) -> ClaudeBackend {
        ClaudeBackend {
            binary: binary.to_string(),
        }
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for ClaudeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Claude
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            spawn: true,
            hide: false,
            attach: true,
            stop: false,
            remove: false,
            rename: true,
        }
    }
    fn list(&mut self) -> Result<Vec<Session>> {
        // A missing/failing binary or non-zero exit is a quiet empty backend, not an error.
        // `--all` includes completed sessions (required to populate the DONE section).
        let output = std::process::Command::new(&self.binary)
            .arg("agents")
            .arg("--json")
            .arg("--all")
            .output();
        let output = match output {
            Ok(output) if output.status.success() => output,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Stream A: for each parsed (session, short_id), read the jobs state.json to fill
        // summary/updated_at_ms/rollout_path. Stage 2 drops the short id.
        let parsed = parse_agents_json(&stdout)?;
        Ok(parsed.into_iter().map(|(session, _short_id)| session).collect())
    }
    fn spawn(&self, dir: &std::path::Path, task: &str) -> Result<Option<u32>> {
        // Detach like the other backends so the TUI key handler returns immediately
        // (`claude --bg` still self-detaches; setsid + no wait keeps it off this thread).
        let name: String = task.chars().take(40).collect();
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.current_dir(dir)
            .arg("--bg")
            .arg("--model")
            .arg("opus[1m]")
            .arg("--name")
            .arg(&name)
            .arg(task);
        let log_path = crate::spawn::viewer_log_path("claude");
        crate::spawn::spawn_detached(cmd, &log_path)?;
        // The forked pid is the dispatcher CLI, not the worker; claude rows are never
        // companions so spawn pinning is unnecessary.
        Ok(None)
    }
    fn rename(&self, session: &Session, name: &str) -> Result<()> {
        let _ = (session, name);
        todo!("Stream A: roster.json rendezvousSock UDS rename_session")
    }
    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.current_dir(&session.cwd).arg("-r").arg(&session.id);
        Some(cmd)
    }
}

/// PURE parser, unit-tested against the fixture. Input: stdout of
/// `claude agents --json --all` — a JSON array of objects.
/// Mapping: id = sessionId (attach takes it); title = name; cwd = cwd;
/// created_at_ms = updated_at_ms = startedAt; hidden = false; source_label = kind;
/// companion = false; summary = "" (filled from state.json by list()).
/// state: "working" -> Working, "blocked" -> NeedsInput, "idle" -> Idle,
/// "done" -> Done, "failed" -> Failed, "stopped" -> Stopped,
/// missing or unknown -> Idle. pid: entry "pid" as u32 when present.
/// The SHORT id (entry "id") the caller needs for the jobs path is returned alongside.
/// Entries missing sessionId/cwd/name are SKIPPED. Non-array top level -> Err(Json).
pub fn parse_agents_json(stdout: &str) -> Result<Vec<(Session, String)>> {
    let _ = stdout;
    todo!("Stream A: six-state mapping + pid + short-id return")
}

/// Parsed subset of a claude jobs `state.json` (verified fields 2026-07-11).
#[derive(Debug, Clone, PartialEq)]
pub struct JobDetail {
    /// needs if state=="blocked" && needs present, else detail, else "".
    pub summary: String,
    /// NOT parsed from ISO updatedAt — the caller uses the state.json file mtime.
    pub updated_at_ms: Option<i64>,
    /// linkScanPath (verified field).
    pub transcript_path: Option<std::path::PathBuf>,
}

/// PURE parse of state.json text (verified fields: state, detail, needs, linkScanPath;
/// blocked jobs carry needs, working/done carry detail).
pub fn parse_job_state(text: &str) -> JobDetail {
    let _ = text;
    todo!("Stream A: blocked-needs-else-detail summary + linkScanPath")
}

/// $HOME/.claude/jobs/<short_id>/state.json
pub fn job_state_path(short_id: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home)
        .join(".claude/jobs")
        .join(short_id)
        .join("state.json")
}

/// Peek parser for the claude session JSONL (verified record shapes 2026-07-11):
/// keep type=="user" (message.content is a STRING, or a list with
/// content[].type=="text") and type=="assistant" (content[].type=="text";
/// "thinking"/tool blocks skipped); skip attachment/system/queue-operation/etc.
/// Return at most the LAST max_items items. Reuses codex::rollout::TranscriptItem.
pub fn read_claude_transcript(
    path: &std::path::Path,
    max_items: usize,
) -> Result<Vec<crate::codex::rollout::TranscriptItem>> {
    let _ = (path, max_items);
    todo!("Stream A: claude session JSONL peek parse")
}
