//! MutationRunner — runs backend mutations (remove/stop/rename/hide) on a worker thread
//! so the render loop never blocks. Each op is a self-contained closure returning a
//! structured outcome; results are drained non-blocking via `poll()`. In-flight keys
//! are deduped so a repeated keypress while an op is pending is a no-op.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use agent_viewer_core::BackendKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSelection {
    pub backend: BackendKind,
    pub session_id: Option<String>,
    pub cwd: PathBuf,
    pub spawned_at_ms: i64,
    pub preexisting_ids: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationOutcome {
    pub notice: String,
    pub spawned: Option<SpawnSelection>,
}

pub struct MutationRunner {
    tx: Sender<(String, Result<MutationOutcome, String>)>,
    rx: Receiver<(String, Result<MutationOutcome, String>)>,
    in_flight: HashSet<String>,
}

impl Default for MutationRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl MutationRunner {
    pub fn new() -> MutationRunner {
        let (tx, rx) = channel();
        MutationRunner {
            tx,
            rx,
            in_flight: HashSet::new(),
        }
    }

    /// Run `op` on a worker thread under `key`. Returns false (a no-op) when `key` is
    /// already in flight.
    pub fn submit<F>(&mut self, key: String, op: F) -> bool
    where
        F: FnOnce() -> Result<MutationOutcome, String> + Send + 'static,
    {
        if self.in_flight.contains(&key) {
            return false;
        }
        self.in_flight.insert(key.clone());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = op();
            // A closed receiver just means the TUI is shutting down; drop the result.
            let _ = tx.send((key, result));
        });
        true
    }

    /// Drain one completed result if ready (non-blocking), clearing its in-flight key.
    pub fn poll(&mut self) -> Option<Result<MutationOutcome, String>> {
        match self.rx.try_recv() {
            Ok((key, result)) => {
                self.in_flight.remove(&key);
                Some(result)
            }
            Err(_) => None,
        }
    }

    pub fn in_flight(&self, key: &str) -> bool {
        self.in_flight.contains(key)
    }
}
