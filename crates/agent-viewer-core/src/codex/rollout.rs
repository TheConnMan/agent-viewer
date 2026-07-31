use crate::backend::TailEvent;
use crate::error::{Error, Result};
use std::io::BufRead;
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

/// Read at most the final 64 KiB of `path` as text (lossy UTF-8). Shared by
/// `tail_state` and `pending_approval` so both classify over the identical window.
pub(crate) fn tail_window(path: &std::path::Path) -> Result<String> {
    crate::read_tail_window(path, 64 * 1024)
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
    let text = tail_window(path)?;
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
            // `turn_aborted` is terminal exactly like `task_complete`: a turn stopped by
            // `turn/interrupt` (or Esc in a TUI) writes NEITHER a task_complete NOR a later
            // task_started, so without this the tail stays MidTurn and the row reads Working
            // forever - a stop would look like it did nothing. Captured live from an
            // interrupted daemon-hosted thread: {"type":"event_msg","payload":
            // {"type":"turn_aborted","turn_id":...,"reason":"interrupted",...}}.
            Some("task_complete") | Some("turn_aborted") => last_complete = Some(idx),
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

/// A pending approval request parsed from the rollout tail (the "what is this session
/// waiting on" surfaced in the peek header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingApproval {
    /// A shell command awaiting approval (exec_approval_request).
    Exec {
        command: Vec<String>,
        cwd: Option<String>,
    },
    /// A patch awaiting approval (apply_patch_approval_request); `files` are the changed paths.
    Patch { files: Vec<String> },
}

/// Single-quote-wrap an argv element that would otherwise misrepresent what runs. Quote
/// anything not in a conservative shell-safe allowlist (`[A-Za-z0-9._/=:@,+-]`) or empty, so
/// metacharacters like `$`, `;`, `*` and whitespace are shown quoted rather than bare (which
/// would misstate the argv the user approves against). Embedded single quotes are escaped as
/// the usual `'\''` (close quote, escaped quote, reopen quote). Plain args pass through bare so
/// a simple command still renders cleanly.
fn shell_quote(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "._/=:@,+-".contains(c);
    let needs_quote = arg.is_empty() || !arg.chars().all(safe);
    if !needs_quote {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

impl PendingApproval {
    /// A one-line human summary for the peek header (no "Awaiting approval:" prefix — the
    /// caller adds it). Exec -> the joined command (each arg shell-quoted so whitespace args
    /// are not misread); Patch -> the changed files (or a generic).
    pub fn summary(&self) -> String {
        match self {
            PendingApproval::Exec { command, .. } => command
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" "),
            PendingApproval::Patch { files } if files.is_empty() => "apply patch".to_string(),
            PendingApproval::Patch { files } => format!("apply patch: {}", files.join(", ")),
        }
    }
}

/// Parse the LAST still-pending approval request from the rollout tail. Uses the same 64 KiB
/// tail window and the same "pending" rule as `tail_state` rule 1 (an approval after the last
/// `task_started` with no later `task_complete`). Ok(None) when nothing is pending or the
/// approval type is unrecognized. Recognizes `exec_approval_request` (payload.command []string,
/// payload.cwd string) and `apply_patch_approval_request` (payload.changes is a map of changed
/// path -> change; the keys are the files). Never panics.
pub fn pending_approval(path: &std::path::Path) -> Result<Option<PendingApproval>> {
    let text = tail_window(path)?;
    let mut last_complete: Option<usize> = None;
    let mut last_started: Option<usize> = None;
    // (line index, parsed content) — Some for a recognized type, None for any other
    // `*_approval_request`, so an exotic type still trips the pending gate but yields None.
    let mut last_approval: Option<(usize, Option<PendingApproval>)> = None;
    for (idx, line) in text.lines().enumerate() {
        let Some(value) = crate::parse_json_line(line) else {
            continue;
        };
        if crate::json_str(&value, "type") != Some("event_msg") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        match crate::json_str(payload, "type") {
            // Same terminal pair as `tail_state`: an aborted turn clears a pending approval,
            // otherwise the peek header keeps asking about a turn that is over.
            Some("task_complete") | Some("turn_aborted") => last_complete = Some(idx),
            Some("task_started") => last_started = Some(idx),
            Some("exec_approval_request") => {
                let command = payload
                    .get("command")
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let cwd = crate::json_str(payload, "cwd").map(|s| s.to_string());
                last_approval = Some((idx, Some(PendingApproval::Exec { command, cwd })));
            }
            Some("apply_patch_approval_request") => {
                let mut files: Vec<String> = payload
                    .get("changes")
                    .and_then(|c| c.as_object())
                    .map(|map| map.keys().cloned().collect())
                    .unwrap_or_default();
                files.sort();
                last_approval = Some((idx, Some(PendingApproval::Patch { files })));
            }
            Some(t) if t.ends_with("_approval_request") => last_approval = Some((idx, None)),
            _ => {}
        }
    }
    // Same pending gate as tail_state rule 1: an approval after the last task_started with
    // no later task_complete.
    if let Some((approval_idx, content)) = last_approval {
        let after_started = last_started.is_none_or(|s| approval_idx > s);
        let no_complete_after = last_complete.is_none_or(|c| c < approval_idx);
        if after_started && no_complete_after {
            return Ok(content);
        }
    }
    Ok(None)
}

/// Codex opens every thread with a `role: "user"` item that is not the user's: the plugin
/// catalogue, the whole of AGENTS.md, and an `<environment_context>` block naming the cwd,
/// shell and date. It is scaffolding wearing the user's role, so the `developer`/`system`
/// filter below does not catch it.
///
/// Measured on this box 2026-07-31: the blob was ~11 KB and was the FIRST tail item of a fresh
/// thread, so the pane opened on a plugin list instead of the task.
///
/// Narrow on BOTH axes, because this filter deletes conversation and a false positive eats the
/// user's own words:
///
/// - the signature is the `<environment_context>` tag codex composes itself, not "looks long"
///   or "mentions AGENTS.md";
/// - and only the LEADING item counts. The measurement found the blob at the head of the
///   thread and nowhere else, so a later message carrying that tag is a human quoting it, and
///   showing a preamble is a far smaller failure than silently swallowing a real prompt.
///
/// "Leading" is the first item of the read WINDOW, which for a transcript longer than
/// `TRANSCRIPT_TAIL_BYTES` is no longer the first item of the thread. That only matters for a
/// mid-conversation message that both quotes the tag and happens to land at the window head.
fn is_environment_preamble(text: &str, emitted_so_far: usize) -> bool {
    emitted_so_far == 0 && text.contains("<environment_context>")
}

/// Parse the END of a rollout for the tail pane, in transcript order. Two kinds of
/// response_item survive: a message (payload.role + payload.content[], concatenating
/// content[].text where content[].type is "input_text" or "output_text") and a tool call
/// (payload.type "function_call" or "custom_tool_call", whose `arguments` is a JSON *string*).
/// Skip malformed lines silently.
///
/// Only the final `TRANSCRIPT_TAIL_BYTES` are read. This used to parse the whole file on every
/// 2s tick to render twelve events, which on a live 18.6 MB rollout is the single most expensive
/// thing the viewer did.
pub fn read_transcript_tail(path: &std::path::Path) -> Result<Vec<TailEvent>> {
    let window = crate::read_tail_window(path, crate::TRANSCRIPT_TAIL_BYTES)?;
    let mut items = Vec::new();
    for line in window.lines() {
        let Some(value) = crate::parse_json_line(line) else {
            continue;
        };
        if crate::json_str(&value, "type") != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if matches!(
            crate::json_str(payload, "type"),
            Some("function_call") | Some("custom_tool_call")
        ) {
            let Some(name) = crate::json_str(payload, "name").filter(|name| !name.is_empty())
            else {
                continue;
            };
            // `arguments` holds the input as an embedded JSON string; a custom tool call
            // carries its raw `input` string instead.
            let detail = crate::json_str(payload, "arguments")
                .and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok())
                .map(|arguments| crate::backend::tool_detail(&arguments))
                .or_else(|| crate::json_str(payload, "input").map(crate::backend::squash))
                .unwrap_or_default();
            items.push(TailEvent::Tool {
                name: name.to_string(),
                detail,
            });
            continue;
        }
        // `developer` and `system` messages are harness scaffolding, not conversation: the
        // memory preamble, the multi-agent prompt, the plugin list. Measured on this box
        // 2026-07-31, a real 6-item rollout was half developer blobs, so a tail that kept
        // them would open on "## Memory You have access to a memory folder ..." instead of
        // the task. The claude parser drops its equivalents the same way.
        let role = match crate::json_str(payload, "role") {
            Some(role @ ("user" | "assistant")) => role,
            _ => continue,
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
        // Tool-only response_items extract to "" — skip them so the pane never shows a
        // blank role-only line.
        if !text.is_empty() && !is_environment_preamble(&text, items.len()) {
            items.push(if role == "user" {
                TailEvent::User(text)
            } else {
                TailEvent::Agent(text)
            });
        }
    }
    Ok(items)
}

/// Read turn timestamps from the full rollout history.
pub(crate) fn read_turn_activity(
    path: &std::path::Path,
    window: std::time::Duration,
) -> Result<Vec<i64>> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let (cutoff, now) = crate::activity_window(window);
    let mut timestamps = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line);
        let Some(value) = crate::parse_json_line(&line) else {
            continue;
        };
        if crate::json_str(&value, "type") != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let text_activity = matches!(
            crate::json_str(payload, "role"),
            Some("user") | Some("assistant")
        ) && matches!(crate::json_str(payload, "type"), Some("message") | None)
            && payload
                .get("content")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|content| {
                    content.iter().any(|chunk| {
                        matches!(
                            crate::json_str(chunk, "type"),
                            Some("input_text") | Some("output_text")
                        ) && crate::json_str(chunk, "text").is_some_and(|text| !text.is_empty())
                    })
                });
        let named_call = matches!(
            crate::json_str(payload, "type"),
            Some("function_call") | Some("custom_tool_call")
        ) && crate::json_str(payload, "name").is_some_and(|name| !name.is_empty());
        let meaningful = text_activity || named_call;
        if !meaningful {
            continue;
        }
        let Some(timestamp) = crate::json_str(&value, "timestamp")
            .and_then(crate::rfc3339_millis)
            .filter(|timestamp| (*timestamp >= cutoff) && (*timestamp <= now))
        else {
            continue;
        };
        timestamps.push(timestamp);
    }
    timestamps.sort_unstable();
    Ok(timestamps)
}
