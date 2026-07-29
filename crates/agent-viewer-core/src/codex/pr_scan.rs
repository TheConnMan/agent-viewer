//! PR refs for codex sessions, scanned out of the rollout JSONL.
//!
//! The codex registry has no PR column, and `threads.git_branch` is captured when the thread
//! starts, so it is stale the moment the agent branches (the thread that opened
//! `curie/pull/1089` still reports `task/fix-interactive-clippy`). The transcript is the only
//! place the PR actually appears, so the badge is sourced from there.
//!
//! Cost is the whole design. `list` runs every second over every thread in the registry
//! (4,956 here, 417 MB of rollouts), so this keeps a per-file offset: a file is read once,
//! then only its appended bytes, and never at all while its length is unchanged. A cold pass
//! is additionally bounded by a per-tick byte budget, so a fresh viewer trickles through the
//! backlog over a few ticks instead of reading every rollout on the box in one.

use crate::backend::PrRef;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Most PRs kept per session. Every ref costs a live `gh` fetch in the TUI's status cache,
/// and a batch-review session can mention over a hundred distinct PRs.
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

/// PURE: every distinct github PR link in `text`, canonicalized to
/// `https://github.com/<owner>/<repo>/pull/<n>` so a deep link (`/pull/12/files`) and a bare
/// one dedupe to a single ref. Non-github hosts, non-pull paths, and `api.github.com` are
/// skipped by `parse_pr_url`'s shape check.
pub fn extract_pr_refs(text: &str) -> Vec<PrRef> {
    let mut refs: Vec<PrRef> = Vec::new();
    scan_pr_keys(text, |owner, repo, number| {
        push_capped(&mut refs, owner, repo, number)
    });
    refs
}

/// Append a ref unless its canonical href is already present, holding the list at
/// `MAX_REFS_PER_SESSION` by dropping the oldest — the PR a session opened lands late in the
/// transcript, after any PRs it merely read.
fn push_capped(refs: &mut Vec<PrRef>, owner: String, repo: String, number: u64) {
    let href = format!("https://github.com/{owner}/{repo}/pull/{number}");
    if refs.iter().any(|r| r.href.as_deref() == Some(href.as_str())) {
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
                    scan_pr_keys(&text, |owner, repo, number| {
                        push_capped(&mut entry.refs, owner, repo, number)
                    });
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
