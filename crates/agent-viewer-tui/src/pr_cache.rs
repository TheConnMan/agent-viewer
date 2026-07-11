//! PrStatusCache — a background PR-status cache modeled on `MutationRunner`: each resolvable
//! PR key gets a `gh` fetch on a worker thread, results drain non-blocking via `poll()`, and
//! the render path reads cached colors purely. In-flight keys are deduped so a key already
//! being fetched is never re-spawned. Entries re-fetch once past the TTL.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use agent_viewer_core::pr_status::{aggregate_color, fetch_pr_status, parse_pr_url};
use agent_viewer_core::{PrBadgeColor, PrRef};

/// (owner, repo, number).
pub type PrKey = (String, String, u64);

/// Re-fetch a PR's status at most this often.
const TTL_MS: i64 = 30_000;

/// A cached PR color plus the ms it was fetched (for the TTL staleness check).
struct Entry {
    color: PrBadgeColor,
    fetched_at_ms: i64,
}

pub struct PrStatusCache {
    entries: HashMap<PrKey, Entry>,
    in_flight: HashSet<PrKey>,
    /// Cloned into each worker thread that request_refs spawns.
    tx: Sender<(PrKey, PrBadgeColor)>,
    rx: Receiver<(PrKey, PrBadgeColor)>,
}

impl Default for PrStatusCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PrStatusCache {
    pub fn new() -> PrStatusCache {
        let (tx, rx) = channel();
        PrStatusCache {
            entries: HashMap::new(),
            in_flight: HashSet::new(),
            tx,
            rx,
        }
    }

    /// Drain completed background fetches into the cache, stamping fetched_at = now_ms.
    pub fn poll(&mut self, now_ms: i64) {
        while let Ok((key, color)) = self.rx.try_recv() {
            self.in_flight.remove(&key);
            self.entries.insert(
                key,
                Entry {
                    color,
                    fetched_at_ms: now_ms,
                },
            );
        }
    }

    /// Parse each ref's href and, for every resolvable PR key that is missing or stale
    /// and not already in flight, spawn a background `gh` fetch.
    pub fn request_refs(&mut self, refs: &[PrRef], now_ms: i64) {
        for r in refs {
            let Some(href) = r.href.as_deref() else {
                continue;
            };
            if let Some(key) = parse_pr_url(href) {
                self.request(key, now_ms);
            }
        }
    }

    /// Spawn a background `gh` fetch for one key, deduping in-flight keys and skipping fresh
    /// entries. Mirrors MutationRunner's thread-per-op + channel pattern.
    fn request(&mut self, key: PrKey, now_ms: i64) {
        if self.in_flight.contains(&key) {
            return;
        }
        if let Some(entry) = self.entries.get(&key)
            && !is_stale(entry.fetched_at_ms, now_ms)
        {
            return;
        }
        self.in_flight.insert(key.clone());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let (owner, repo, number) = &key;
            let color = fetch_pr_status(owner, repo, *number);
            // A closed receiver just means the TUI is shutting down; drop the result.
            let _ = tx.send((key, color));
        });
    }

    /// Cached color for a key, or Default if unknown. Pure read (render path).
    pub fn color(&self, key: &PrKey) -> PrBadgeColor {
        self.entries
            .get(key)
            .map(|e| e.color)
            .unwrap_or(PrBadgeColor::Default)
    }

    /// Aggregate badge color for a session's PR refs (parse href -> key -> cached color ->
    /// aggregate). Refs with no resolvable github href contribute nothing. Pure read.
    pub fn badge_color(&self, refs: &[PrRef]) -> PrBadgeColor {
        let colors: Vec<PrBadgeColor> = refs
            .iter()
            .filter_map(|r| r.href.as_deref())
            .filter_map(parse_pr_url)
            .map(|key| self.color(&key))
            .collect();
        aggregate_color(&colors)
    }
}

/// True once an entry has aged past the TTL. Pure so it is unit-testable.
pub(crate) fn is_stale(fetched_at_ms: i64, now_ms: i64) -> bool {
    now_ms - fetched_at_ms >= TTL_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    // E) is_stale — true iff now - fetched_at >= TTL_MS (30_000).

    #[test]
    fn is_stale_just_under_ttl_is_fresh() {
        assert!(!is_stale(0, 29_999));
    }

    #[test]
    fn is_stale_exactly_ttl_is_stale() {
        assert!(is_stale(0, 30_000));
    }

    #[test]
    fn is_stale_well_past_ttl_is_stale() {
        assert!(is_stale(0, 40_000));
    }

    #[test]
    fn is_stale_zero_elapsed_is_fresh() {
        assert!(!is_stale(1_000, 1_000));
    }
}
