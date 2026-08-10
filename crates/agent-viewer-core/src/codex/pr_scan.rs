//! PR refs for codex sessions, scanned out of the rollout JSONL.
//!
//! The codex registry has no PR column, and `threads.git_branch` is captured when the thread
//! starts, so it is stale the moment the agent branches (the thread that opened
//! `example/pull/1089` still reports `task/fix-interactive-clippy`). The transcript is the only
//! place the PR actually appears, so the badge is sourced from there.
//!
//! Cost is the whole design. `list` runs every second over every thread in the registry
//! (4,956 here, 417 MB of rollouts), so this keeps a per-file offset: a file is read once,
//! then only its appended bytes, and never at all while its length is unchanged. A cold pass
//! is additionally bounded by a per-tick byte budget, so a fresh viewer trickles through the
//! backlog over a few ticks instead of reading every rollout on the box in one.

use crate::backend::PrRef;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Most successfully created PRs kept per session. Every ref costs a live `gh` fetch in the
/// TUI's status cache, and one session can create more PRs than the viewer should retain.
pub const MAX_REFS_PER_SESSION: usize = 4;

/// Bytes of rollout the scanner may read per listing tick, across all sessions.
pub const SCAN_BUDGET_BYTES: u64 = 32 * 1024 * 1024;

/// Bytes legal in the path part of a github PR URL. Anything else (a quote, comma,
/// backslash, paren, whitespace) ends the candidate — rollout URLs are embedded in JSON, so
/// the delimiter is nearly always one of those.
fn is_url_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/')
}

/// Call `on_key` with every github PR link in `text`, in the order they appear, as
/// (owner, repo, number). Deduping and capping are the caller's, because they accumulate
/// across the whole file, not one chunk.
fn scan_pr_keys(text: &str, mut on_key: impl FnMut(String, String, u64)) {
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for prefix in ["https://github.com/", "http://github.com/"] {
        hits.extend(text.match_indices(prefix).map(|(at, _)| (at, prefix.len())));
    }
    hits.sort_unstable();
    let bytes = text.as_bytes();
    for (start, prefix_len) in hits {
        let mut end = start + prefix_len;
        while end < bytes.len() && is_url_path_byte(bytes[end]) {
            end += 1;
        }
        if let Some((owner, repo, number)) = crate::pr_status::parse_pr_url(&text[start..end]) {
            on_key(owner, repo, number);
        }
    }
}

/// Append a ref unless its canonical href is already present, holding the list at
/// `MAX_REFS_PER_SESSION` by dropping the oldest successful creation. This retains the most
/// recently created refs when one session creates many PRs.
fn push_capped(refs: &mut Vec<PrRef>, owner: String, repo: String, number: u64) {
    let href = format!("https://github.com/{owner}/{repo}/pull/{number}");
    if refs
        .iter()
        .any(|r| r.href.as_deref() == Some(href.as_str()))
    {
        return;
    }
    refs.push(PrRef {
        id: number.to_string(),
        href: Some(href),
    });
    if refs.len() > MAX_REFS_PER_SESSION {
        refs.remove(0);
    }
}

/// What one `exec` result says: whether the command failed, and the command's own output.
///
/// Codex writes an exec result in two shapes, both live on this box. The common one is plain
/// text content items, a `Script completed` / `Script failed` header followed by the raw
/// command output (`gh pr create` prints just the PR URL). The other appears when the result
/// is chunked or truncated: the item text is itself a JSON object carrying `exit_code` and
/// `output`. Reading only the JSON shape badges nothing at all, because the creation results
/// this box actually records are plain text.
struct ExecResult {
    failed: bool,
    command_output: Vec<String>,
    /// Shell session ids the result reports. A command that outlives its call returns one of
    /// these with empty output, and the rest of its output (the PR URL) arrives under a later
    /// call that polls the same id.
    shell_sessions: Vec<u64>,
}

impl ExecResult {
    fn parse(payload: &serde_json::Map<String, Value>) -> Option<ExecResult> {
        let output = payload.get("output")?;
        // `output` is an array of content items, or, less often, a bare string.
        let texts: Vec<&str> = match output {
            Value::Array(items) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect(),
            Value::String(text) => vec![text.as_str()],
            _ => return None,
        };

        let mut result = ExecResult {
            failed: false,
            command_output: Vec::new(),
            shell_sessions: Vec::new(),
        };
        for text in texts {
            match serde_json::from_str::<Value>(text) {
                Ok(Value::Object(chunk)) => {
                    if chunk
                        .get("exit_code")
                        .and_then(Value::as_i64)
                        .is_some_and(|code| code != 0)
                    {
                        result.failed = true;
                    }
                    if let Some(session_id) = chunk.get("session_id").and_then(Value::as_u64) {
                        result.shell_sessions.push(session_id);
                    }
                    if let Some(command_output) = chunk.get("output").and_then(Value::as_str) {
                        result.command_output.push(command_output.to_owned());
                    }
                }
                _ => {
                    if text.starts_with("Script failed") {
                        result.failed = true;
                    }
                    result.command_output.push(text.to_owned());
                }
            }
        }
        Some(result)
    }
}

/// Whether `input` polls one of the shell sessions in `sessions`, i.e. carries
/// `session_id: <id>` in any of the spacing and quoting the tool call JS uses.
fn polls_shell_session(input: &str, sessions: &HashSet<u64>) -> bool {
    if sessions.is_empty() {
        return false;
    }
    input.match_indices("session_id").any(|(at, key)| {
        let rest = input[at + key.len()..]
            .trim_start_matches(['"', '\'', ' ', ':'])
            .as_bytes();
        let digits: String = rest
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .map(|byte| *byte as char)
            .collect();
        digits.parse::<u64>().is_ok_and(|id| sessions.contains(&id))
    })
}

/// Scan a `mcp_tool_call_end` event for a PR opened through the GitHub connector, the other
/// way a thread creates one: `tools.mcp__codex_apps__github_create_pull_request`. The tool
/// result the model sees is only `Action completed.`, so the URL is read off the event's own
/// structured result, which is as strong a provenance signal as a `gh pr create` exit code.
fn scan_connector_pr_create(payload: &serde_json::Map<String, Value>, entry: &mut Entry) {
    let tool = payload
        .get("invocation")
        .and_then(|invocation| invocation.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !tool.ends_with("create_pull_request") {
        return;
    }
    let Some(content) = payload
        .get("result")
        .and_then(|result| result.get("Ok"))
        .and_then(|ok| ok.get("structuredContent"))
    else {
        return;
    };
    if content.get("isError").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let Some(url) = content.get("url").and_then(Value::as_str) else {
        return;
    };
    scan_pr_keys(url, |owner, repo, number| {
        push_capped(&mut entry.refs, owner, repo, number)
    });
}

/// Scan one complete rollout line. A PR is attributed to the session only when the thread
/// itself opened it: an `exec` call creating one paired with its successful result, or a
/// successful GitHub connector creation. Rollout prose and unrelated tool output may contain
/// arbitrary PR links and are deliberately ignored.
fn scan_rollout_line(line: &str, entry: &mut Entry) {
    let Ok(item) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let kind = item.get("type").and_then(Value::as_str);
    let Some(payload) = item.get("payload").and_then(Value::as_object) else {
        return;
    };
    if kind == Some("event_msg") {
        if payload.get("type").and_then(Value::as_str) == Some("mcp_tool_call_end") {
            scan_connector_pr_create(payload, entry);
        }
        return;
    }
    if kind != Some("response_item") {
        return;
    }

    match payload.get("type").and_then(Value::as_str) {
        Some("custom_tool_call") => {
            if payload.get("name").and_then(Value::as_str) != Some("exec") {
                return;
            }
            let Some(input) = payload.get("input").and_then(Value::as_str) else {
                return;
            };
            let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                return;
            };
            if input.contains("gh pr create")
                || polls_shell_session(input, &entry.pending_pr_shells)
            {
                entry.pending_pr_creates.insert(call_id.to_owned());
            }
        }
        Some("custom_tool_call_output") => {
            let Some(call_id) = payload.get("call_id").and_then(Value::as_str) else {
                return;
            };
            if !entry.pending_pr_creates.remove(call_id) {
                return;
            }
            let Some(result) = ExecResult::parse(payload) else {
                return;
            };
            if result.failed {
                return;
            }
            let before = entry.refs.len();
            for text in &result.command_output {
                scan_pr_keys(text, |owner, repo, number| {
                    push_capped(&mut entry.refs, owner, repo, number)
                });
            }
            // A `gh pr create` that outlived its call returned a shell session and no URL yet.
            // Follow that session so the poll carrying the URL is read as part of the creation.
            if entry.refs.len() == before {
                entry.pending_pr_shells.extend(result.shell_sessions);
            } else {
                for session in result.shell_sessions {
                    entry.pending_pr_shells.remove(&session);
                }
            }
        }
        _ => {}
    }
}

/// What the scanner remembers about one rollout file.
#[derive(Default)]
struct Entry {
    /// File length at the last scan attempt. Equal length means nothing was appended, which
    /// is the check that makes the steady state free.
    scanned_len: u64,
    /// Byte offset of the first not-yet-scanned COMPLETE line. A rollout is appended to
    /// live, so the trailing partial line is deliberately left unscanned: parsing it would
    /// mint a truncated PR number, and refs are sticky, so that badge would never heal.
    offset: u64,
    refs: Vec<PrRef>,
    /// `gh pr create` exec calls whose matching result has not arrived yet. This must persist
    /// across ticks because a call and its output commonly land in separate appended chunks.
    pending_pr_creates: HashSet<String>,
    /// Shell sessions running a `gh pr create` that has not printed its URL yet. A later call
    /// polling one of these is treated as the same creation.
    pending_pr_shells: HashSet<u64>,
}

/// Incremental per-rollout PR-ref scanner. Held by the codex backend across ticks.
#[derive(Default)]
pub struct PrScanner {
    entries: HashMap<PathBuf, Entry>,
}

impl PrScanner {
    pub fn new() -> PrScanner {
        PrScanner::default()
    }

    /// The PRs mentioned in `path`, reading only what has been appended since the last call
    /// and charging the bytes read to `budget`. A missing or unreadable rollout is empty and
    /// free (most registry rows point at a pruned file). An exhausted budget defers the read
    /// to a later tick rather than dropping it.
    pub fn refs_for(&mut self, path: &Path, budget: &mut u64) -> Vec<PrRef> {
        let Ok(len) = std::fs::metadata(path).map(|meta| meta.len()) else {
            return self.cached(path);
        };
        let mut entry = self.entries.remove(path).unwrap_or_default();
        // A shorter file is a different file at the same path (rotation or a rewrite):
        // keeping the offset would skip its whole head forever.
        if len < entry.offset {
            entry = Entry::default();
        }
        if entry.scanned_len == len {
            let refs = entry.refs.clone();
            self.entries.insert(path.to_path_buf(), entry);
            return refs;
        }
        if *budget == 0 {
            let refs = entry.refs.clone();
            self.entries.insert(path.to_path_buf(), entry);
            return refs;
        }
        match read_from(path, entry.offset) {
            Ok(buf) => {
                *budget = budget.saturating_sub(buf.len() as u64);
                // Length as READ, not as stat'd: the file may have grown between the two, and
                // charging the smaller number would re-read those bytes every tick.
                entry.scanned_len = entry.offset + buf.len() as u64;
                if let Some(last_newline) = buf.iter().rposition(|byte| *byte == b'\n') {
                    let text = String::from_utf8_lossy(&buf[..=last_newline]);
                    for line in text.lines() {
                        scan_rollout_line(line, &mut entry);
                    }
                    entry.offset += last_newline as u64 + 1;
                }
            }
            // Unreadable this tick (permissions, a vanished file): keep what we have and
            // retry next tick rather than caching a wrong empty.
            Err(_) => {
                let refs = entry.refs.clone();
                self.entries.insert(path.to_path_buf(), entry);
                return refs;
            }
        }
        let refs = entry.refs.clone();
        self.entries.insert(path.to_path_buf(), entry);
        refs
    }

    /// Whatever is already known for `path`, without touching the filesystem.
    fn cached(&self, path: &Path) -> Vec<PrRef> {
        self.entries
            .get(path)
            .map(|entry| entry.refs.clone())
            .unwrap_or_default()
    }
}

/// Read `path` from `offset` to EOF.
fn read_from(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}
