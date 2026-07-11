use crate::backend::Status;

/// IMPURE scanner (live-e2e-verified only, no unit tests): enumerate processes via
/// sysinfo whose name starts with "codex", read /proc/<pid>/fd/* via
/// std::fs::read_link, collect target paths. Strip a trailing " (deleted)" suffix.
/// Unreadable /proc entries are skipped silently (other-user procs, races).
pub fn open_rollout_paths() -> std::collections::HashSet<std::path::PathBuf> {
    todo!()
}

/// PURE resolution given the open set (all unit tests target this):
/// 1. canonicalize rollout_path (fall back to the raw path on error);
///    if it is in open_paths -> Running  (running wins over task_complete).
/// 2. else has_task_complete_tail == Ok(true) -> Done.
/// 3. else (Ok(false) OR any read error, incl. missing file) -> Errored. Never panics.
pub fn resolve_status(
    rollout_path: &std::path::Path,
    open_paths: &std::collections::HashSet<std::path::PathBuf>,
) -> Status {
    let _ = (rollout_path, open_paths);
    todo!()
}

/// Caching wrapper for the refresh loop. Cache key: (mtime, len) of rollout_path.
/// Recompute when the key changes, when the path is in open_paths, or on first sight.
pub struct StatusResolver {
    _cache: std::collections::HashMap<std::path::PathBuf, ((std::time::SystemTime, u64), Status)>,
}

impl StatusResolver {
    pub fn new() -> StatusResolver {
        todo!()
    }
    pub fn resolve(
        &mut self,
        rollout_path: &std::path::Path,
        open_paths: &std::collections::HashSet<std::path::PathBuf>,
    ) -> Status {
        let _ = (rollout_path, open_paths);
        todo!()
    }
}

impl Default for StatusResolver {
    fn default() -> Self {
        Self::new()
    }
}
