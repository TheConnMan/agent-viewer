use crate::backend::{Backend, BackendKind, Capabilities, Session, Status};
use crate::error::{Error, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

pub struct ClaudeBackend {
    binary: String,
    /// Per-job state.json cache keyed by (mtime, len): the file is re-read and re-parsed
    /// only when it changes, not every tick (mirrors the codex StatusResolver pattern).
    detail_cache: HashMap<PathBuf, ((SystemTime, u64), JobDetail)>,
}

impl ClaudeBackend {
    pub fn new() -> ClaudeBackend {
        ClaudeBackend {
            binary: "claude".to_string(),
            detail_cache: HashMap::new(),
        }
    }
    pub fn with_binary(binary: &str) -> ClaudeBackend {
        ClaudeBackend {
            binary: binary.to_string(),
            detail_cache: HashMap::new(),
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
        let parsed = parse_agents_json(&stdout)?;
        let mut sessions = Vec::with_capacity(parsed.len());
        for mut session in parsed {
            // Fill summary/updated_at_ms/rollout_path from the jobs state.json. A
            // missing/garbled file is not an error — the jobs dir can lag the agents list.
            // The parse is cached by (mtime, len) so an unchanged file is not re-read.
            let short_id = session.short_id.clone().unwrap_or_default();
            let path = job_state_path(&short_id);
            if let Ok(meta) = std::fs::metadata(&path) {
                let key = (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len());
                let detail = match self.detail_cache.get(&path) {
                    Some((cached_key, detail)) if *cached_key == key => Some(detail.clone()),
                    _ => std::fs::read_to_string(&path).ok().map(|text| {
                        let detail = parse_job_state(&text);
                        self.detail_cache.insert(path.clone(), (key, detail.clone()));
                        detail
                    }),
                };
                if let Some(detail) = detail {
                    session.summary = detail.summary;
                    session.rollout_path = detail.transcript_path;
                    if let Ok(since) = key.0.duration_since(std::time::UNIX_EPOCH) {
                        session.updated_at_ms = since.as_millis() as i64;
                    }
                }
            }
            sessions.push(session);
        }
        Ok(sessions)
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
        // Best-effort UDS rename against the live daemon worker (unofficial Fleet View
        // protocol). Any failure -> Err so the TUI falls back to set_name_override.
        use std::io::{Read, Write};
        let home = std::env::var("HOME").unwrap_or_default();
        let roster_path = std::path::PathBuf::from(&home).join(".claude/daemon/roster.json");
        let text = std::fs::read_to_string(&roster_path)?;
        let roster: serde_json::Value = serde_json::from_str(&text)?;
        let sock = roster
            .get("workers")
            .and_then(|w| w.as_object())
            .and_then(|workers| {
                workers.values().find(|worker| {
                    worker.get("sessionId").and_then(|s| s.as_str()) == Some(session.id.as_str())
                })
            })
            .and_then(|worker| worker.get("rendezvousSock"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| Error::Command("no live worker for session".into()))?;
        let mut stream = std::os::unix::net::UnixStream::connect(sock)?;
        let mut line = serde_json::json!({ "subtype": "rename_session", "title": name }).to_string();
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        // The reply is advisory; a successful write is the success signal.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
        let mut buf = [0u8; 256];
        let _ = stream.read(&mut buf);
        Ok(())
    }
    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new(&self.binary);
        // A live session (pid present, or a Working/NeedsInput state) opens Claude Code's
        // agent view with this job preselected and expanded — the view is not tied to the
        // session cwd, so it runs from $HOME. A finished session resumes by full id,
        // pinned to its cwd only when that dir still exists.
        let live = session.pid.is_some()
            || matches!(session.status, Status::Working | Status::NeedsInput);
        if live {
            let short_id = session.short_id.clone().unwrap_or_default();
            let home = std::env::var("HOME").unwrap_or_default();
            cmd.arg("agents")
                .env("CLAUDE_AGENTS_SELECT", short_id)
                .current_dir(home);
        } else {
            cmd.arg("-r").arg(&session.id);
            if session.cwd.is_dir() {
                cmd.current_dir(&session.cwd);
            }
        }
        Some(cmd)
    }
}

/// Pre-accept the claude trust dialog for `cwd` in the config JSON at `config_path`, so a
/// non-interactive attach into a fresh project does not stall on the trust prompt. Returns
/// Ok(false) (no write) when `cwd` or any ancestor is already accepted, Ok(true) when the
/// flag was merged in (or the file was created). Every other key at every level is
/// preserved; the write is atomic (temp file in the same dir + rename). This is claude's
/// own sanctioned field — its error text instructs setting exactly `hasTrustDialogAccepted`
/// for non-interactive use.
pub fn ensure_trusted(config_path: &std::path::Path, cwd: &std::path::Path) -> Result<bool> {
    let mut config: serde_json::Value = match std::fs::read_to_string(config_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({})),
        // Missing file -> start from a minimal structure and create it.
        Err(_) => serde_json::json!({}),
    };

    // Already trusted if cwd or any ancestor has hasTrustDialogAccepted == true.
    if let Some(projects) = config.get("projects").and_then(|p| p.as_object()) {
        let mut dir = Some(cwd);
        while let Some(d) = dir {
            let accepted = projects
                .get(&d.to_string_lossy().into_owned())
                .and_then(|p| p.get("hasTrustDialogAccepted"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if accepted {
                return Ok(false);
            }
            dir = d.parent();
        }
    }

    // Merge hasTrustDialogAccepted: true into projects[<cwd>], preserving every other key.
    let projects = config
        .as_object_mut()
        .expect("json object")
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    if !projects.is_object() {
        *projects = serde_json::json!({});
    }
    let cwd_key = cwd.to_string_lossy().into_owned();
    let project = projects
        .as_object_mut()
        .expect("projects object")
        .entry(cwd_key)
        .or_insert_with(|| serde_json::json!({}));
    if !project.is_object() {
        *project = serde_json::json!({});
    }
    project
        .as_object_mut()
        .expect("project object")
        .insert("hasTrustDialogAccepted".to_string(), serde_json::json!(true));

    // Atomic write: temp file in the same dir, then rename over the target.
    let text = serde_json::to_string_pretty(&config)?;
    let dir = config_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        config_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "claude.json".to_string())
    ));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, config_path)?;
    Ok(true)
}

/// PURE parser, unit-tested against the fixture. Input: stdout of
/// `claude agents --json --all` — a JSON array of objects.
/// Mapping: id = sessionId (attach takes it); title = name; cwd = cwd;
/// created_at_ms = updated_at_ms = startedAt; hidden = false; source_label = kind;
/// companion = false; summary = "" (filled from state.json by list()).
/// state: "working" -> Working, "blocked" -> NeedsInput, "idle" -> Idle,
/// "done" -> Done, "failed" -> Failed, "stopped" -> Stopped,
/// missing or unknown -> Idle. pid: entry "pid" as u32 when present.
/// The SHORT id (entry "id") the caller needs for the jobs path is folded into
/// `Session.short_id`. Entries missing sessionId/cwd/name are SKIPPED. Non-array top
/// level -> Err(Json).
pub fn parse_agents_json(stdout: &str) -> Result<Vec<Session>> {
    // Non-array top level surfaces as Err(Json) via the From conversion.
    let entries: Vec<serde_json::Value> = serde_json::from_str(stdout.trim())?;
    let mut sessions = Vec::with_capacity(entries.len());
    for entry in entries {
        // sessionId/cwd/name are required; anything missing them is skipped.
        let Some(session_id) = entry.get("sessionId").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let short_id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let started_at = entry.get("startedAt").and_then(|v| v.as_i64()).unwrap_or(0);
        let source_label = entry
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let status = match entry.get("state").and_then(|v| v.as_str()) {
            Some("working") => Status::Working,
            Some("blocked") => Status::NeedsInput,
            Some("idle") => Status::Idle,
            Some("done") => Status::Done,
            Some("failed") => Status::Failed,
            Some("stopped") => Status::Stopped,
            // Missing or unknown state -> Idle (verified live: some entries have no state).
            _ => Status::Idle,
        };
        let pid = entry.get("pid").and_then(|v| v.as_u64()).map(|p| p as u32);
        sessions.push(Session {
            backend: BackendKind::Claude,
            id: session_id.to_string(),
            short_id: Some(short_id),
            title: name.to_string(),
            cwd: std::path::PathBuf::from(cwd),
            created_at_ms: started_at,
            updated_at_ms: started_at,
            status,
            hidden: false,
            source_label,
            summary: String::new(),
            companion: false,
            pid,
            rollout_path: None,
        });
    }
    Ok(sessions)
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
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return JobDetail {
            summary: String::new(),
            updated_at_ms: None,
            transcript_path: None,
        };
    };
    let state = value.get("state").and_then(|v| v.as_str());
    let needs = value.get("needs").and_then(|v| v.as_str());
    let detail = value.get("detail").and_then(|v| v.as_str());
    let summary = if state == Some("blocked")
        && let Some(needs) = needs
    {
        needs.to_string()
    } else if let Some(detail) = detail {
        detail.to_string()
    } else {
        String::new()
    };
    let transcript_path = value
        .get("linkScanPath")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);
    JobDetail {
        summary,
        updated_at_ms: None,
        transcript_path,
    }
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
    use crate::codex::rollout::TranscriptItem;
    use std::io::BufRead;

    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut items = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let role = match value.get("type").and_then(|t| t.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            // attachment/system/queue-operation/etc. are skipped.
            _ => continue,
        };
        let content = value.get("message").and_then(|m| m.get("content"));
        let text = match content {
            // message.content is either a plain string...
            Some(serde_json::Value::String(s)) => s.clone(),
            // ...or a list of blocks; keep only type=="text" (thinking/tool skipped).
            Some(serde_json::Value::Array(blocks)) => {
                let mut text = String::new();
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text")
                        && let Some(t) = block.get("text").and_then(|t| t.as_str())
                    {
                        text.push_str(t);
                    }
                }
                text
            }
            _ => continue,
        };
        items.push(TranscriptItem {
            role: role.to_string(),
            text,
        });
    }
    if items.len() > max_items {
        items = items.split_off(items.len() - max_items);
    }
    Ok(items)
}
