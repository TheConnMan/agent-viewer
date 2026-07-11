use crate::backend::Status;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// IMPURE scanner (live-e2e-verified only, no unit tests): enumerate processes via
/// sysinfo whose name starts with "codex", read /proc/<pid>/fd/* via
/// std::fs::read_link, collect target paths. Strip a trailing " (deleted)" suffix.
/// Unreadable /proc entries are skipped silently (other-user procs, races).
pub fn open_rollout_paths() -> HashSet<PathBuf> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut open = HashSet::new();
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
            open.insert(PathBuf::from(stripped));
        }
    }
    open
}

/// PURE resolution given the open set (all unit tests target this):
/// 1. canonicalize rollout_path (fall back to the raw path on error);
///    if it is in open_paths -> Running  (running wins over task_complete).
/// 2. else has_task_complete_tail == Ok(true) -> Done.
/// 3. else (Ok(false) OR any read error, incl. missing file) -> Errored. Never panics.
pub fn resolve_status(rollout_path: &Path, open_paths: &HashSet<PathBuf>) -> Status {
    let canonical =
        std::fs::canonicalize(rollout_path).unwrap_or_else(|_| rollout_path.to_path_buf());
    if open_paths.contains(&canonical) {
        return Status::Running;
    }
    match super::rollout::has_task_complete_tail(rollout_path) {
        Ok(true) => Status::Done,
        _ => Status::Errored,
    }
}

/// Caching wrapper for the refresh loop. Cache key: (mtime, len) of rollout_path.
/// Recompute when the key changes, when the path is in open_paths, or on first sight.
pub struct StatusResolver {
    cache: HashMap<PathBuf, ((SystemTime, u64), Status)>,
}

impl StatusResolver {
    pub fn new() -> StatusResolver {
        StatusResolver {
            cache: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, rollout_path: &Path, open_paths: &HashSet<PathBuf>) -> Status {
        let canonical =
            std::fs::canonicalize(rollout_path).unwrap_or_else(|_| rollout_path.to_path_buf());
        if open_paths.contains(&canonical) {
            return Status::Running;
        }
        let key = std::fs::metadata(rollout_path).ok().map(|meta| {
            (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            )
        });
        let Some(key) = key else {
            // No metadata (missing file): resolve_status yields Errored, nothing to cache.
            return resolve_status(rollout_path, open_paths);
        };
        if let Some((cached_key, cached_status)) = self.cache.get(rollout_path)
            && *cached_key == key
        {
            return *cached_status;
        }
        let status = resolve_status(rollout_path, open_paths);
        self.cache.insert(rollout_path.to_path_buf(), (key, status));
        status
    }
}

impl Default for StatusResolver {
    fn default() -> Self {
        Self::new()
    }
}
