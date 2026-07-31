//! ModelCache: the composer's model catalog, kept off the render loop. Discovery is a CLI
//! shell-out (a cold probe can take seconds), so it runs on a worker thread modeled
//! on `PrStatusCache`: results drain non-blocking via `poll()`, and the key path only ever
//! reads memory. Lists are seeded from the viewer DB at startup, so after the first run the
//! picker is populated on the first keystroke instead of after a multi-second probe.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use agent_viewer_core::backend::{BackendKind, all_backends};

/// Re-probe a backend's catalog at most once a day. Model catalogs change on the scale of
/// CLI releases, and a stale list still spawns fine (the id is passed straight through).
const TTL_MS: i64 = 24 * 60 * 60 * 1000;

pub struct ModelCache {
    /// Discovered lists in picker order, keyed by backend.
    entries: HashMap<BackendKind, Vec<String>>,
    /// Backends already probed, or seeded fresh enough not to need it. Probing is a
    /// multi-second shell-out and `request` is called on every keystroke, so a backend gets
    /// at most one probe per viewer session, including when that probe found nothing.
    attempted: HashSet<BackendKind>,
    /// Cloned into each worker thread `request_with` spawns.
    tx: Sender<(BackendKind, Vec<String>)>,
    rx: Receiver<(BackendKind, Vec<String>)>,
}

impl Default for ModelCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCache {
    pub fn new() -> ModelCache {
        let (tx, rx) = channel();
        ModelCache {
            entries: HashMap::new(),
            attempted: HashSet::new(),
            tx,
            rx,
        }
    }

    /// Install a list read from the viewer DB. A stale row (`fresh` false) still populates
    /// the picker (an old catalog beats an empty one), but leaves the backend open to a
    /// refresh that replaces it when it lands.
    pub fn seed(&mut self, backend: BackendKind, models: Vec<String>, fresh: bool) {
        if models.is_empty() {
            return;
        }
        self.entries.insert(backend, models);
        if fresh {
            self.attempted.insert(backend);
        }
    }

    /// Cached list for a backend, or None when nothing has been discovered for it yet.
    /// Pure read (the composer's install path).
    pub fn models(&self, backend: BackendKind) -> Option<&[String]> {
        self.entries.get(&backend).map(|m| m.as_slice())
    }

    /// Discover `backend`'s catalog on a worker thread, unless it has been probed or freshly
    /// seeded already.
    pub fn request(&mut self, backend: BackendKind) {
        self.request_with(backend, move || probe(backend));
    }

    /// `request` with the discovery call injected, so the cache's dedup and hand-off can be
    /// tested without shelling out to a real CLI.
    pub fn request_with<F>(&mut self, backend: BackendKind, probe: F)
    where
        F: FnOnce() -> Vec<String> + Send + 'static,
    {
        if !self.attempted.insert(backend) {
            return;
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            // A closed receiver just means the TUI is shutting down; drop the result.
            let _ = tx.send((backend, probe()));
        });
    }

    /// Drain completed probes into the cache. Returns the landed lists so the caller can
    /// persist them and refresh the composer; a probe that discovered nothing is dropped
    /// here, so neither the picker nor the DB records a failure.
    pub fn poll(&mut self) -> Vec<(BackendKind, Vec<String>)> {
        let mut landed = Vec::new();
        while let Ok((backend, models)) = self.rx.try_recv() {
            if !discovered_something(&models) {
                continue;
            }
            self.entries.insert(backend, models.clone());
            landed.push((backend, models));
        }
        landed
    }
}

/// A catalog is only worth keeping if it holds more than the built-in default: every
/// `available_models` seeds that first, so a one-entry list means the CLI probe failed.
fn discovered_something(models: &[String]) -> bool {
    models.len() > 1
}

/// True once a stored catalog has aged past the TTL. Pure so it is unit-testable, and
/// tolerant of a stamp in the future (a clock that moved backwards must not pin a row stale).
pub fn is_stale(fetched_at_ms: i64, now_ms: i64) -> bool {
    now_ms - fetched_at_ms >= TTL_MS
}

/// The real discovery call: build a fresh backend of this kind on the worker thread (the
/// UI's backends are not ours to move) and ask it for its models.
fn probe(backend: BackendKind) -> Vec<String> {
    all_backends()
        .iter()
        .find(|b| b.kind() == backend)
        .map(|b| b.available_models())
        .unwrap_or_default()
}
