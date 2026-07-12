//! The peek transcript-tail cache: `PeekCache` re-reads the focused session's backing
//! transcript only when its (path, mtime, len) fingerprint changes. Moved verbatim out of
//! `ui` so the rendering module holds only render functions.

use agent_viewer_core::codex::rollout::{TranscriptItem, read_transcript};
use agent_viewer_core::{BackendKind, Session};
use std::path::{Path, PathBuf};

/// Cap on transcript items retained for peek (only the tail is ever shown).
const MAX_TRANSCRIPT_ITEMS: usize = 200;

// --- Peek cache (backend-dispatching transcript tail) ---------------------------

/// Cache key: backend + transcript path + its (mtime, len) fingerprint.
type PeekKey = (BackendKind, PathBuf, Option<(u64, u64)>);

/// Cached transcript tail for the peek overlay. Re-reads only when the focused
/// session's backing file (path, mtime, len) key changes, so the per-frame cost is
/// one stat() (opencode has no file — metadata is rendered live).
pub struct PeekCache {
    key: Option<PeekKey>,
    // items/error/ask are read by `ui`'s peek renderer, so they are pub(crate) now that the
    // cache lives in its own module (the fields were private when it was a `ui` sibling).
    pub(crate) items: Vec<TranscriptItem>,
    pub(crate) error: Option<String>,
    /// Codex pending-approval summary (the "what is this waiting on" ask), when the focused
    /// session has one. None for non-codex backends and when nothing is pending.
    pub(crate) ask: Option<String>,
}

impl Default for PeekCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PeekCache {
    pub fn new() -> Self {
        PeekCache {
            key: None,
            items: Vec::new(),
            error: None,
            ask: None,
        }
    }

    /// Point the cache at the focused session. codex -> rollout transcript tail,
    /// claude -> session-JSONL tail, opencode (no path) -> metadata rendered live.
    pub fn refresh(&mut self, session: Option<&Session>) {
        let Some(session) = session else {
            self.clear();
            return;
        };
        let Some(path) = session.rollout_path.as_deref() else {
            if session.backend == BackendKind::Opencode {
                self.refresh_opencode(session);
            } else {
                // No transcript file and not opencode: metadata fallback only.
                self.clear();
            }
            return;
        };
        let fkey = file_key(path);
        let key = Some((session.backend, path.to_path_buf(), fkey));
        if self.key == key {
            return;
        }
        self.key = key;
        let read = match session.backend {
            BackendKind::Claude => {
                agent_viewer_core::claude::read_claude_transcript(path, MAX_TRANSCRIPT_ITEMS)
            }
            _ => read_transcript(path),
        };
        match read {
            Ok(mut items) => {
                if items.len() > MAX_TRANSCRIPT_ITEMS {
                    items.drain(0..items.len() - MAX_TRANSCRIPT_ITEMS);
                }
                self.items = items;
                self.error = None;
                // Codex surfaces the pending approval (if any) as the ask; other backends
                // derive their ask elsewhere (claude from session.summary, opencode none).
                self.ask = if session.backend == BackendKind::Codex {
                    agent_viewer_core::codex::rollout::pending_approval(path)
                        .ok()
                        .flatten()
                        .map(|a| a.summary())
                } else {
                    None
                };
            }
            Err(e) => {
                self.items.clear();
                self.error = Some(format!("transcript unavailable: {e}"));
                self.ask = None;
            }
        }
    }

    /// opencode has no transcript file — the last message lives in the SQLite `message`/
    /// `part` tables. Key off the session id + its updated_at_ms so we re-read only when the
    /// session advances (there is no file mtime to stat). The synthetic file part of the key
    /// is (updated_at_ms, 0).
    fn refresh_opencode(&mut self, session: &Session) {
        let key = Some((
            session.backend,
            PathBuf::from(&session.id),
            Some((session.updated_at_ms.max(0) as u64, 0)),
        ));
        if self.key == key {
            return;
        }
        self.key = key;
        self.ask = None;
        match agent_viewer_core::opencode::read_opencode_last_message(
            &agent_viewer_core::opencode::default_opencode_db(),
            &session.id,
        ) {
            Ok(Some(item)) => {
                self.items = vec![item];
                self.error = None;
            }
            Ok(None) => {
                self.items.clear();
                self.error = None;
            }
            Err(e) => {
                self.items.clear();
                self.error = Some(format!("transcript unavailable: {e}"));
            }
        }
    }

    fn clear(&mut self) {
        self.key = None;
        self.items.clear();
        self.error = None;
        self.ask = None;
    }
}

fn file_key(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Some((mtime, meta.len()))
}
