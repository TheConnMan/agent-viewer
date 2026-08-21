//! ModelCache: the composer's model catalog, kept off the render loop. Discovery is a CLI
//! shell-out (a cold probe can take seconds), so it runs on a worker thread modeled
//! on `PrStatusCache`: results drain non-blocking via `poll()`, and the key path only ever
//! reads memory. Lists are seeded from the viewer DB at startup, so after the first run the
//! picker is populated on the first keystroke instead of after a multi-second probe.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use agent_viewer_core::backend::{BackendKind, all_backends};

use crate::composer::{CommandEntry, file_stems, subdir_names};

pub type CommandCacheKey = (BackendKind, Option<PathBuf>);

enum CommandProbeEvent {
    Catalog(CommandCacheKey, Vec<CommandEntry>),
    Finished(CommandCacheKey),
}

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
    /// Provider and target scoped command catalogs. Failed refreshes never replace a usable
    /// entry, so legacy prompts and filesystem skills remain available when daemon discovery
    /// returns nothing.
    command_entries: HashMap<CommandCacheKey, Vec<CommandEntry>>,
    command_attempted: HashSet<CommandCacheKey>,
    command_pending: HashSet<CommandCacheKey>,
    command_tx: Sender<CommandProbeEvent>,
    command_rx: Receiver<CommandProbeEvent>,
}

impl Default for ModelCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCache {
    pub fn new() -> ModelCache {
        let (tx, rx) = channel();
        let (command_tx, command_rx) = channel();
        ModelCache {
            entries: HashMap::new(),
            attempted: HashSet::new(),
            tx,
            rx,
            command_entries: HashMap::new(),
            command_attempted: HashSet::new(),
            command_pending: HashSet::new(),
            command_tx,
            command_rx,
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

    /// Return a command catalog already discovered for this provider and target. This is a
    /// memory read only and is safe on key and render adjacent paths.
    pub fn commands(&self, key: &CommandCacheKey) -> Option<&[CommandEntry]> {
        self.command_entries.get(key).map(Vec::as_slice)
    }

    pub fn commands_pending(&self, key: &CommandCacheKey) -> bool {
        self.command_pending.contains(key)
    }

    /// Start provider discovery once for a target. All filesystem work and Codex daemon RPC
    /// stays on this worker; callers only submit the request and read cached results. Returns
    /// true only when this call starts the worker.
    pub fn request_commands(&mut self, key: CommandCacheKey) -> bool {
        self.request_commands_with_codex_discovery(key, agent_viewer_core::codex::discover_skills)
    }

    /// Command discovery with the blocking Codex lookup injected at its external boundary.
    /// The worker, local fallback, normalization, and cache handoff are identical to production.
    #[doc(hidden)]
    pub fn request_commands_with_codex_discovery<F>(
        &mut self,
        key: CommandCacheKey,
        discover_codex_skills: F,
    ) -> bool
    where
        F: FnOnce(&std::path::Path) -> Vec<agent_viewer_core::codex::app_server::CodexSkill>
            + Send
            + 'static,
    {
        self.request_commands_with(key, move |key, tx| {
            let mut entries = probe_local_commands(&key);
            normalize_commands(&mut entries);
            if !entries.is_empty() {
                let _ = tx.send(CommandProbeEvent::Catalog(key.clone(), entries.clone()));
            }
            if key.0 == BackendKind::Codex
                && let Some(target) = key.1.as_deref()
            {
                entries.extend(
                    discover_codex_skills(target)
                        .into_iter()
                        .map(|skill| CommandEntry::codex_skill(skill.name, skill.path)),
                );
                normalize_commands(&mut entries);
                let _ = tx.send(CommandProbeEvent::Catalog(key, entries));
            } else if entries.is_empty() {
                let _ = tx.send(CommandProbeEvent::Catalog(key, entries));
            }
        })
    }

    fn request_commands_with<F>(&mut self, key: CommandCacheKey, probe: F) -> bool
    where
        F: FnOnce(CommandCacheKey, Sender<CommandProbeEvent>) + Send + 'static,
    {
        if !self.command_attempted.insert(key.clone()) {
            return false;
        }
        self.command_pending.insert(key.clone());
        let tx = self.command_tx.clone();
        thread::spawn(move || {
            let finished_key = key.clone();
            let finished_tx = tx.clone();
            let _ = catch_unwind(AssertUnwindSafe(|| probe(key, tx)));
            let _ = finished_tx.send(CommandProbeEvent::Finished(finished_key));
        });
        true
    }

    /// Drain completed command probes. Empty results do not evict a previous usable list.
    /// Returning landed keys lets the caller reinstall the active union without polling or
    /// touching the filesystem from the UI thread.
    pub fn poll_commands(&mut self) -> Vec<CommandCacheKey> {
        let mut landed = Vec::new();
        while let Ok(event) = self.command_rx.try_recv() {
            match event {
                CommandProbeEvent::Catalog(key, entries) => {
                    if entries.is_empty() {
                        continue;
                    }
                    self.command_entries.insert(key.clone(), entries);
                    landed.push(key);
                }
                CommandProbeEvent::Finished(key) => {
                    self.command_pending.remove(&key);
                }
            }
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

fn probe_local_commands(key: &CommandCacheKey) -> Vec<CommandEntry> {
    let (backend, target) = key;
    let home = agent_viewer_core::home_dir();
    match backend {
        BackendKind::Claude => {
            let mut names = subdir_names(&home.join(".claude/skills"));
            if let Some(target) = target {
                names.extend(subdir_names(&target.join(".claude/skills")));
            }
            names
                .into_iter()
                .map(CommandEntry::claude_skill)
                .collect::<Vec<_>>()
        }
        BackendKind::Codex => file_stems(&home.join(".codex/prompts"))
            .into_iter()
            .map(CommandEntry::codex_prompt)
            .collect::<Vec<_>>(),
    }
}

fn normalize_commands(entries: &mut Vec<CommandEntry>) {
    entries.sort_by(|left, right| {
        left.display()
            .cmp(right.display())
            .then_with(|| {
                left.owner()
                    .map(BackendKind::name)
                    .cmp(&right.owner().map(BackendKind::name))
            })
            .then_with(|| left.kind().cmp(&right.kind()))
            .then_with(|| left.codex_skill_path().cmp(&right.codex_skill_path()))
    });
    entries.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    #[test]
    fn empty_refresh_does_not_evict_a_usable_local_catalog() {
        let mut cache = ModelCache::new();
        let key = (
            BackendKind::Codex,
            Some(PathBuf::from("/tmp/agentviewer-command-cache")),
        );
        let fallback = CommandEntry::codex_prompt("review");
        cache
            .command_entries
            .insert(key.clone(), vec![fallback.clone()]);
        cache
            .command_tx
            .send(CommandProbeEvent::Catalog(key.clone(), Vec::new()))
            .expect("queue empty refresh");

        assert!(cache.poll_commands().is_empty());
        assert_eq!(cache.commands(&key), Some([fallback].as_slice()));
    }

    #[test]
    fn repeated_command_request_starts_only_one_worker() {
        let mut cache = ModelCache::new();
        let key = (
            BackendKind::Claude,
            Some(PathBuf::from("/tmp/agentviewer-command-dedup")),
        );
        let (started_tx, started_rx) = channel();

        for (index, name) in ["first", "second", "third"].into_iter().enumerate() {
            let started_tx = started_tx.clone();
            let started = cache.request_commands_with(key.clone(), move |key, results| {
                started_tx.send(()).expect("record worker start");
                let _ = results.send(CommandProbeEvent::Catalog(
                    key,
                    vec![CommandEntry::claude_skill(name)],
                ));
            });
            assert_eq!(started, index == 0);
        }
        drop(started_tx);

        assert_eq!(started_rx.recv_timeout(Duration::from_secs(1)), Ok(()));
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected),
            "a repeated request for one target must not fan out another worker"
        );
    }

    #[test]
    fn empty_and_failed_command_discovery_clear_pending() {
        for fail in [false, true] {
            let mut cache = ModelCache::new();
            let key = (
                BackendKind::Codex,
                Some(PathBuf::from("/tmp/agentviewer_command_pending")),
            );
            let (started_tx, started_rx) = channel();
            cache.request_commands_with(key.clone(), move |key, results| {
                started_tx.send(()).expect("record worker start");
                if fail {
                    return;
                }
                let _ = results.send(CommandProbeEvent::Catalog(key, Vec::new()));
            });

            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("worker starts");
            assert!(cache.commands_pending(&key));
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while cache.commands_pending(&key) {
                cache.poll_commands();
                assert!(
                    std::time::Instant::now() < deadline,
                    "completed discovery remained pending"
                );
                std::thread::yield_now();
            }
            assert_eq!(cache.commands(&key), None);
        }
    }
}
