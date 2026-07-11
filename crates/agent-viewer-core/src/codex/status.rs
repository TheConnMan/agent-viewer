use super::rollout::TailState;
use crate::backend::Status;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// IMPURE scanner (live-e2e-verified only, no unit tests): enumerate processes via
/// sysinfo whose name starts with "codex", read /proc/<pid>/fd/* via
/// std::fs::read_link, collect target paths -> owning codex PID. Strip a trailing
/// " (deleted)" suffix. Unreadable /proc entries are skipped silently.
pub fn open_rollout_paths() -> HashMap<PathBuf, u32> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut open = HashMap::new();
    for (pid, process) in sys.processes() {
        if !process.name().to_string_lossy().starts_with("codex") {
            continue;
        }
        let fd_dir = format!("/proc/{}/fd", pid.as_u32());
        let Ok(entries) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let display = target.to_string_lossy();
            let stripped = display.strip_suffix(" (deleted)").unwrap_or(&display);
            open.insert(PathBuf::from(stripped), pid.as_u32());
        }
    }
    open
}

/// PURE six-state resolution given the open map (all unit tests target this).
/// Canonicalization exactly as v1. Rules (section 5.3):
///   open + tail AwaitingApproval          -> NeedsInput
///   open + tail MidTurn OR unreadable     -> Working   (spawn race: empty file)
///   open + tail Complete                  -> Idle      (live session between turns)
///   closed + Complete                     -> Done
///   closed + anything else               -> Failed    (v1 Errored, renamed)
/// Stopped is NOT resolved here — it is a viewer-DB overlay (section 5.7). Never panics.
pub fn resolve_status(rollout_path: &Path, open_paths: &HashMap<PathBuf, u32>) -> Status {
    let canonical =
        std::fs::canonicalize(rollout_path).unwrap_or_else(|_| rollout_path.to_path_buf());
    let open = open_paths.contains_key(&canonical);
    match (open, super::rollout::tail_state(rollout_path)) {
        (true, Ok(TailState::AwaitingApproval)) => Status::NeedsInput,
        (true, Ok(TailState::Complete)) => Status::Idle,
        // MidTurn or an unreadable/empty file (spawn race) -> Working while held open.
        (true, _) => Status::Working,
        (false, Ok(TailState::Complete)) => Status::Done,
        // Closed and not cleanly complete (MidTurn, awaiting, or unreadable) -> Failed.
        (false, _) => Status::Failed,
    }
}

/// Caching wrapper for the refresh loop. Cache key: (mtime, len) of rollout_path.
/// Recompute when the key changes, when the path is in open_paths, or on first sight.
pub struct StatusResolver {
    cache: HashMap<PathBuf, ((SystemTime, u64), Status)>,
    canonical: HashMap<PathBuf, PathBuf>,
}

impl StatusResolver {
    pub fn new() -> StatusResolver {
        StatusResolver {
            cache: HashMap::new(),
            canonical: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, rollout_path: &Path, open_paths: &HashMap<PathBuf, u32>) -> Status {
        // canonicalize once per distinct rollout_path, then reuse across ticks. Cache ONLY
        // successful canonicalizations: an Err (path not present yet) uses the raw path for
        // this tick without caching, so a later tick retries once the file appears.
        let canonical = match self.canonical.get(rollout_path) {
            Some(c) => c.clone(),
            None => match std::fs::canonicalize(rollout_path) {
                Ok(c) => {
                    self.canonical.insert(rollout_path.to_path_buf(), c.clone());
                    c
                }
                Err(_) => rollout_path.to_path_buf(),
            },
        };
        // Open sessions always recompute (their tail changes turn-to-turn). Closed
        // sessions reuse the (mtime, len) cache — the hot path this struct exists for
        // (prior intent, commit 49a7c1b; 2,883 threads on this box).
        let open = open_paths.contains_key(&canonical);
        let key = std::fs::metadata(rollout_path).ok().map(|meta| {
            (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            )
        });
        if !open
            && let Some(key) = key
            && let Some((cached_key, cached_status)) = self.cache.get(rollout_path)
            && *cached_key == key
        {
            return *cached_status;
        }
        let status = resolve_status(rollout_path, open_paths);
        if let Some(key) = key {
            self.cache.insert(rollout_path.to_path_buf(), (key, status));
        }
        status
    }
}

impl Default for StatusResolver {
    fn default() -> Self {
        Self::new()
    }
}
