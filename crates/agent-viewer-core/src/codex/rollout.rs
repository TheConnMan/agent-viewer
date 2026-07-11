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
    if crate::json_str(&value, "type") != Some("session_meta") {
        return Err(Error::Command("first line is not session_meta".into()));
    }
    let payload = value
        .get("payload")
        .ok_or_else(|| Error::Command("session_meta missing payload".into()))?;
    let field = |key: &str| {
        crate::json_str(payload, key)
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

/// The last-turn outcome derived from the rollout tail (replaces v1's
/// `has_task_complete_tail`). See section 5.2 of the v2 plan for the decision order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailState {
    Complete,
    MidTurn,
    AwaitingApproval,
}

/// Read at most the final 64 KiB and classify the last turn. Tracks the last line
/// index of `task_complete`, `task_started`, and any `event_msg` whose payload.type
/// ENDS WITH `_approval_request` (protocol names `exec_approval_request` /
/// `apply_patch_approval_request`; suffix match absorbs future variants).
/// Decision order:
///   1. approval seen, after the last `task_started`, with no `task_complete` after
///      it -> `AwaitingApproval`
///   2. else v1's last-turn rule verbatim (prior intent, commit ae99791): last
///      `task_complete` after last `task_started` (or complete with no started in
///      window) -> `Complete`
///   3. else -> `MidTurn`
pub fn tail_state(path: &std::path::Path) -> Result<TailState> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let window: u64 = 64 * 1024;
    file.seek(SeekFrom::Start(len.saturating_sub(window)))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut last_complete: Option<usize> = None;
    let mut last_started: Option<usize> = None;
    let mut last_approval: Option<usize> = None;
    for (idx, line) in text.lines().enumerate() {
        let Some(value) = crate::parse_json_line(line) else {
            continue;
        };
        if crate::json_str(&value, "type") != Some("event_msg") {
            continue;
        }
        match value
            .get("payload")
            .and_then(|p| crate::json_str(p, "type"))
        {
            Some("task_complete") => last_complete = Some(idx),
            Some("task_started") => last_started = Some(idx),
            Some(t) if t.ends_with("_approval_request") => last_approval = Some(idx),
            _ => {}
        }
    }
    // Rule 1: an approval after the last task_started with no later task_complete.
    if let Some(approval) = last_approval {
        let after_started = last_started.is_none_or(|s| approval > s);
        let no_complete_after = last_complete.is_none_or(|c| c < approval);
        if after_started && no_complete_after {
            return Ok(TailState::AwaitingApproval);
        }
    }
    // Rule 2: v1's last-turn completion rule verbatim (prior intent, commit ae99791).
    let complete = match (last_complete, last_started) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(c), Some(s)) => c > s,
    };
    if complete {
        Ok(TailState::Complete)
    } else {
        Ok(TailState::MidTurn)
    }
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
        let Some(value) = crate::parse_json_line(&line) else {
            continue;
        };
        if crate::json_str(&value, "type") != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(role) = crate::json_str(payload, "role") else {
            continue;
        };
        let Some(content) = payload.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        let mut text = String::new();
        for chunk in content {
            if matches!(
                crate::json_str(chunk, "type"),
                Some("input_text") | Some("output_text")
            ) && let Some(t) = crate::json_str(chunk, "text")
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
