use crate::error::{Error, Result};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub cwd: std::path::PathBuf,
    pub originator: String,
    pub cli_version: String,
}

/// Parse ONLY the first line (type "session_meta"; fields under .payload).
/// The first line is large (payload.base_instructions) — read_line, do not cap.
/// Empty file or non-session_meta first line -> Err.
pub fn read_session_meta(path: &std::path::Path) -> Result<SessionMeta> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(Error::Command("empty rollout file".into()));
    }
    let value: serde_json::Value = serde_json::from_str(line.trim())?;
    if value.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return Err(Error::Command("first line is not session_meta".into()));
    }
    let payload = value
        .get("payload")
        .ok_or_else(|| Error::Command("session_meta missing payload".into()))?;
    let field = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Ok(SessionMeta {
        id: field("id"),
        cwd: PathBuf::from(field("cwd")),
        originator: field("originator"),
        cli_version: field("cli_version"),
    })
}

/// Terminal marker check. Read at most the final 64 KiB, split to lines, and decide the
/// last turn's outcome by event order: return true iff the LAST `task_complete` event
/// occurs after the LAST `task_started` event (both are event_msg lines with payload.type).
/// If no `task_started` is in the window (its turn start may predate the window), any
/// `task_complete` still counts. A stale `task_complete` followed by a later `task_started`
/// with no new completion (resumed-then-abandoned) resolves to false.
pub fn has_task_complete_tail(path: &std::path::Path) -> Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let window: u64 = 64 * 1024;
    file.seek(SeekFrom::Start(len.saturating_sub(window)))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut last_complete: Option<usize> = None;
    let mut last_started: Option<usize> = None;
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        match value
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(|t| t.as_str())
        {
            Some("task_complete") => last_complete = Some(idx),
            Some("task_started") => last_started = Some(idx),
            _ => {}
        }
    }
    Ok(match (last_complete, last_started) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(complete), Some(started)) => complete > started,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptItem {
    pub role: String,
    pub text: String,
}

/// Full lazy parse for the detail pane. Keep only response_item lines with
/// payload.role + payload.content[]; concatenate content[].text where
/// content[].type is "input_text" or "output_text". Skip malformed lines silently.
pub fn read_transcript(path: &std::path::Path) -> Result<Vec<TranscriptItem>> {
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
        if value.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(role) = payload.get("role").and_then(|r| r.as_str()) else {
            continue;
        };
        let Some(content) = payload.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        let mut text = String::new();
        for chunk in content {
            let chunk_type = chunk.get("type").and_then(|t| t.as_str());
            if matches!(chunk_type, Some("input_text") | Some("output_text"))
                && let Some(t) = chunk.get("text").and_then(|t| t.as_str())
            {
                text.push_str(t);
            }
        }
        items.push(TranscriptItem {
            role: role.to_string(),
            text,
        });
    }
    Ok(items)
}
