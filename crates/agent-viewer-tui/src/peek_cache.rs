//! The peek transcript-tail cache: `PeekCache` re-reads the focused session's backing
//! transcript only when its (path, mtime, len) fingerprint changes. Moved verbatim out of
//! `ui` so the rendering module holds only render functions.

use agent_viewer_core::codex::rollout::{TranscriptItem, read_transcript};
use agent_viewer_core::opencode::OpencodeRuntime;
use agent_viewer_core::{BackendKind, Session, Status};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

/// Cap on transcript items retained for peek (only the tail is ever shown).
const MAX_TRANSCRIPT_ITEMS: usize = 200;

// --- Peek cache (backend-dispatching transcript tail) ---------------------------

/// Cache key: backend + transcript path + its (mtime, len) fingerprint.
type PeekKey = (BackendKind, PathBuf, Option<(u64, u64)>);
type OpencodePeekResult = (PeekKey, Result<Option<TranscriptItem>, String>);

/// Cached transcript tail for the peek overlay. Re-reads only when the focused
/// session's backing file (path, mtime, len) key changes, so the per-frame cost is
/// one stat for file backed sessions. OpenCode reads run on a background worker.
pub struct PeekCache {
    key: Option<PeekKey>,
    opencode_runtime: OpencodeRuntime,
    opencode_result_tx: Sender<OpencodePeekResult>,
    opencode_result_rx: Receiver<OpencodePeekResult>,
    opencode_in_flight: Option<PeekKey>,
    // items/error/ask are read by `ui`'s peek renderer, so they are pub(crate) now that the
    // cache lives in its own module (the fields were private when it was a `ui` sibling).
    pub(crate) items: Vec<TranscriptItem>,
    pub(crate) error: Option<String>,
    /// Codex pending-approval summary (the "what is this waiting on" ask), when the focused
    /// session has one, or the OpenCode pending input reason. None when nothing is pending.
    pub(crate) ask: Option<String>,
}

impl Default for PeekCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PeekCache {
    pub fn new() -> Self {
        Self::with_opencode_runtime(OpencodeRuntime::new())
    }

    pub fn with_opencode_runtime(opencode_runtime: OpencodeRuntime) -> Self {
        let (opencode_result_tx, opencode_result_rx) = channel();
        PeekCache {
            key: None,
            opencode_runtime,
            opencode_result_tx,
            opencode_result_rx,
            opencode_in_flight: None,
            items: Vec::new(),
            error: None,
            ask: None,
        }
    }

    /// Point the cache at the focused session. codex -> rollout transcript tail,
    /// claude -> session-JSONL tail, opencode -> server or SQLite message history.
    pub fn refresh(&mut self, session: Option<&Session>) {
        let Some(session) = session else {
            self.drain_opencode_results(None);
            self.clear();
            return;
        };
        if session.backend == BackendKind::Opencode {
            self.refresh_opencode(session);
            return;
        }
        self.drain_opencode_results(None);
        let Some(path) = session.rollout_path.as_deref() else {
            // No transcript file: metadata fallback only.
            self.clear();
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

    /// OpenCode has no transcript file. Key off the session id and updated time, then let the
    /// shared runtime choose server history or the read only SQLite fallback on a worker.
    fn refresh_opencode(&mut self, session: &Session) {
        let key = (
            session.backend,
            PathBuf::from(&session.id),
            Some((session.updated_at_ms.max(0) as u64, 0)),
        );
        self.drain_opencode_results(Some(&key));
        self.ask = match &session.status {
            Status::NeedsInput { reason } => reason.clone(),
            _ => None,
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = None;
        self.items.clear();
        self.error = None;
        if self.opencode_in_flight.is_some() {
            return;
        }
        self.opencode_in_flight = Some(key.clone());
        let runtime = self.opencode_runtime.clone();
        let result_tx = self.opencode_result_tx.clone();
        let db_path = agent_viewer_core::opencode::default_opencode_db();
        let session_id = session.id.clone();
        std::thread::spawn(move || {
            let result = runtime
                .read_last_message(&db_path, &session_id)
                .map_err(|error| format!("transcript unavailable: {error}"));
            let _ = result_tx.send((key, result));
        });
    }

    fn drain_opencode_results(&mut self, selected: Option<&PeekKey>) {
        while let Ok((key, result)) = self.opencode_result_rx.try_recv() {
            if self.opencode_in_flight.as_ref() == Some(&key) {
                self.opencode_in_flight = None;
            }
            if selected != Some(&key) {
                continue;
            }
            self.key = Some(key);
            match result {
                Ok(Some(item)) => {
                    self.items = vec![item];
                    self.error = None;
                }
                Ok(None) => {
                    self.items.clear();
                    self.error = None;
                }
                Err(error) => {
                    self.items.clear();
                    self.error = Some(error);
                }
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
