use crate::backend::{Backend, BackendKind, Capabilities, Session, Status};
use crate::error::{Error, Result};
use std::path::PathBuf;

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
        }
    }
    fn list(&mut self) -> Result<Vec<Session>> {
        // A missing/failing binary or non-zero exit is a quiet empty backend, not an error.
        let output = std::process::Command::new(&self.binary)
            .arg("agents")
            .arg("--json")
            .output();
        let output = match output {
            Ok(output) if output.status.success() => output,
            _ => return Ok(Vec::new()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_agents_json(&stdout)
    }
    fn spawn(&self, dir: &std::path::Path, task: &str) -> Result<()> {
        // `claude --bg` self-detaches after dispatching the background job; wait for it.
        let name: String = task.chars().take(40).collect();
        let output = std::process::Command::new(&self.binary)
            .current_dir(dir)
            .arg("--bg")
            .arg("--model")
            .arg("opus[1m]")
            .arg("--name")
            .arg(&name)
            .arg(task)
            .output()?;
        if !output.status.success() {
            return Err(Error::Command(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(())
    }
    fn attach_command(&self, session: &Session) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.current_dir(&session.cwd).arg("-r").arg(&session.id);
        Some(cmd)
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
    // Deserializing to Vec yields Err(Json) for a non-array top level.
    let entries: Vec<serde_json::Value> = serde_json::from_str(stdout)?;
    let mut sessions = Vec::new();
    for entry in entries {
        // Entries missing any required field are skipped (schema drift must not kill
        // the whole backend).
        let (Some(session_id), Some(cwd), Some(name)) = (
            entry.get("sessionId").and_then(|v| v.as_str()),
            entry.get("cwd").and_then(|v| v.as_str()),
            entry.get("name").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let started_at = entry.get("startedAt").and_then(|v| v.as_i64()).unwrap_or(0);
        let source_label = entry
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let status = match entry.get("state").and_then(|v| v.as_str()).unwrap_or("") {
            "working" => Status::Running,
            "done" => Status::Done,
            _ => Status::Errored,
        };
        sessions.push(Session {
            backend: BackendKind::Claude,
            id: session_id.to_string(),
            title: name.to_string(),
            cwd: PathBuf::from(cwd),
            created_at_ms: started_at,
            updated_at_ms: started_at,
            status,
            hidden: false,
            source_label,
            rollout_path: None,
        });
    }
    Ok(sessions)
}
