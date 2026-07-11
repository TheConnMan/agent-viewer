//! Viewer-owned state DB (the ONLY SQLite this tool writes) plus the pure overlay and
//! spawn-matching functions. Backends stay ignorant of it; the overlay applies
//! pins/stops/renames to the concatenated session list in the TUI loop.

use crate::backend::{BackendKind, Session};
use crate::error::Result;
use std::collections::{HashMap, HashSet};

/// Handle to `~/.local/state/agent-viewer/viewer.db` (read-write).
pub struct ViewerDb {
    #[allow(dead_code)]
    conn: rusqlite::Connection,
}

/// An unresolved viewer-spawned session record (pin candidate awaiting a session id).
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRecord {
    pub rowid: i64,
    pub backend: BackendKind,
    pub cwd: std::path::PathBuf,
    pub pid: u32,
    pub spawned_at_ms: i64,
}

/// One snapshot read per tick feeding the overlay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewerState {
    /// resolved spawns (always-visible pins)
    pub pinned: HashSet<(BackendKind, String)>,
    pub stopped: HashSet<(BackendKind, String)>,
    pub renames: HashMap<(BackendKind, String), String>,
    pub spawn_pids: HashMap<(BackendKind, String), u32>,
}

impl ViewerDb {
    /// $HOME/.local/state/agent-viewer/viewer.db, parent dir created. Read-WRITE.
    /// On any open/schema failure: delete the file, recreate fresh (all contents are
    /// advisory; losing them costs pins/labels, nothing more).
    pub fn open_default() -> Result<ViewerDb> {
        todo!("Stream A: default path + recreate-on-corrupt open")
    }

    /// tests: temp path.
    pub fn open(path: &std::path::Path) -> Result<ViewerDb> {
        let _ = path;
        todo!("Stream A: open + CREATE IF NOT EXISTS schema, recreate on corrupt")
    }

    pub fn record_spawn(
        &self,
        backend: BackendKind,
        cwd: &std::path::Path,
        pid: u32,
        spawned_at_ms: i64,
    ) -> Result<i64> {
        let _ = (backend, cwd, pid, spawned_at_ms);
        todo!("Stream A: INSERT into spawned, return rowid")
    }

    pub fn resolve_spawn(&self, rowid: i64, session_id: &str) -> Result<()> {
        let _ = (rowid, session_id);
        todo!("Stream A: set session_id on the spawned row")
    }

    /// Unresolved records; the caller deletes any older than 10 min (abandoned).
    pub fn unresolved_spawns(&self) -> Result<Vec<SpawnRecord>> {
        todo!("Stream A: SELECT spawned rows with NULL session_id")
    }

    pub fn delete_spawn(&self, rowid: i64) -> Result<()> {
        let _ = rowid;
        todo!("Stream A: DELETE spawned row")
    }

    pub fn mark_stopped(&self, backend: BackendKind, session_id: &str) -> Result<()> {
        let _ = (backend, session_id);
        todo!("Stream A: INSERT OR REPLACE into stopped")
    }

    pub fn clear_stopped(&self, backend: BackendKind, session_id: &str) -> Result<()> {
        let _ = (backend, session_id);
        todo!("Stream A: DELETE from stopped")
    }

    pub fn set_name_override(
        &self,
        backend: BackendKind,
        session_id: &str,
        name: &str,
    ) -> Result<()> {
        let _ = (backend, session_id, name);
        todo!("Stream A: INSERT OR REPLACE into renames")
    }

    /// One snapshot read per tick feeding the overlay.
    pub fn viewer_state(&self) -> Result<ViewerState> {
        todo!("Stream A: assemble ViewerState from all three tables")
    }
}

/// PURE overlay, applied in the TUI loop after concatenating backend lists:
///   - renames: replace title
///   - pinned: set companion = false (viewer-spawned sessions always visible)
///   - spawn_pids: fill session.pid when None (opencode stop path)
///   - stopped: Done|Failed -> Stopped. If the session is live again
///     (Working/NeedsInput/Idle) the record is STALE — status unchanged, key
///     returned so the caller clears it from the DB.
/// Returns stale stopped keys. Visibility filtering is NOT done here (App owns it).
pub fn apply_viewer_state(
    sessions: &mut [Session],
    state: &ViewerState,
) -> Vec<(BackendKind, String)> {
    let _ = (sessions, state);
    todo!("Stream A: apply renames/pins/pids/stopped, return stale stopped keys")
}

/// PURE spawn matching (runs per tick for unresolved records): a session of
/// record.backend with cwd == record.cwd and created_at_ms within
/// [spawned_at_ms - 2_000, spawned_at_ms + 30_000]; nearest created_at wins on ties.
/// (cwd + time window only — deliberately no pid correlation.)
pub fn match_spawn(record: &SpawnRecord, sessions: &[Session]) -> Option<String> {
    let _ = (record, sessions);
    todo!("Stream A: cwd + time-window nearest-created match")
}
