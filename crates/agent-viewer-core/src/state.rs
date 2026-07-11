//! Viewer-owned state DB (the ONLY SQLite this tool writes) plus the pure overlay and
//! spawn-matching functions. Backends stay ignorant of it; the overlay applies
//! pins/stops/renames to the concatenated session list in the TUI loop.

use crate::backend::{BackendKind, Session, Status};
use crate::error::Result;
use std::collections::{HashMap, HashSet};

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS spawned (\
  id INTEGER PRIMARY KEY,\
  backend TEXT NOT NULL,\
  session_id TEXT,\
  cwd TEXT NOT NULL,\
  pid INTEGER NOT NULL,\
  spawned_at_ms INTEGER NOT NULL\
);\
CREATE TABLE IF NOT EXISTS stopped (\
  backend TEXT NOT NULL,\
  session_id TEXT NOT NULL,\
  stopped_at_ms INTEGER NOT NULL,\
  PRIMARY KEY(backend, session_id)\
);\
CREATE TABLE IF NOT EXISTS renames (\
  backend TEXT NOT NULL,\
  session_id TEXT NOT NULL,\
  name TEXT NOT NULL,\
  PRIMARY KEY(backend, session_id)\
);";

/// "codex" | "claude" | "opencode" back into a BackendKind (None for unknown text).
fn backend_from_str(s: &str) -> Option<BackendKind> {
    match s {
        "codex" => Some(BackendKind::Codex),
        "claude" => Some(BackendKind::Claude),
        "opencode" => Some(BackendKind::Opencode),
        _ => None,
    }
}

/// Handle to `~/.local/state/agent-viewer/viewer.db` (read-write).
pub struct ViewerDb {
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
        ViewerDb::open(&crate::home_dir().join(".local/state/agent-viewer/viewer.db"))
    }

    /// tests: temp path.
    pub fn open(path: &std::path::Path) -> Result<ViewerDb> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match ViewerDb::try_open(path) {
            Ok(db) => Ok(db),
            // Any open/schema failure means the file is unusable (corrupt or not a DB).
            // The contents are advisory, so replace it with a fresh empty DB.
            Err(_) => {
                let _ = std::fs::remove_file(path);
                ViewerDb::try_open(path)
            }
        }
    }

    fn try_open(path: &std::path::Path) -> Result<ViewerDb> {
        let conn = rusqlite::Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        // WAL so two concurrent viewers coexist (last-writer-wins on advisory state).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(ViewerDb { conn })
    }

    /// Prune resolved spawn rows older than 7 days whose (backend, session_id) is NOT in
    /// `live`. Resolved rows are always-visible pins; once their session is gone AND the
    /// row is stale they only grow the table and the per-tick overlay scan. The live check
    /// is the safety belt: a still-existing session keeps its pin no matter how old the row
    /// is, so a long-running spawned session never loses its always-visible status.
    /// (Unresolved rows are handled by the caller's abandonment rule.)
    pub fn prune_resolved_missing(&self, live: &HashSet<(BackendKind, String)>) -> Result<()> {
        let cutoff = crate::spawn::now_ms() - 7 * 24 * 60 * 60 * 1000;
        let mut stmt = self.conn.prepare(
            "SELECT id, backend, session_id FROM spawned \
             WHERE session_id IS NOT NULL AND spawned_at_ms < ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![cutoff], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut doomed = Vec::new();
        for row in rows {
            let (rowid, backend, session_id) = row?;
            let Some(backend) = backend_from_str(&backend) else {
                continue;
            };
            if !live.contains(&(backend, session_id)) {
                doomed.push(rowid);
            }
        }
        for rowid in doomed {
            self.conn.execute(
                "DELETE FROM spawned WHERE id = ?1",
                rusqlite::params![rowid],
            )?;
        }
        Ok(())
    }

    pub fn record_spawn(
        &self,
        backend: BackendKind,
        cwd: &std::path::Path,
        pid: u32,
        spawned_at_ms: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO spawned (backend, session_id, cwd, pid, spawned_at_ms) \
             VALUES (?1, NULL, ?2, ?3, ?4)",
            rusqlite::params![
                backend.name(),
                cwd.to_string_lossy(),
                pid as i64,
                spawned_at_ms
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn resolve_spawn(&self, rowid: i64, session_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE spawned SET session_id = ?1 WHERE id = ?2",
            rusqlite::params![session_id, rowid],
        )?;
        Ok(())
    }

    /// Unresolved records; the caller deletes any older than 10 min (abandoned).
    pub fn unresolved_spawns(&self) -> Result<Vec<SpawnRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, backend, cwd, pid, spawned_at_ms FROM spawned WHERE session_id IS NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (rowid, backend, cwd, pid, spawned_at_ms) = row?;
            let Some(backend) = backend_from_str(&backend) else {
                continue;
            };
            records.push(SpawnRecord {
                rowid,
                backend,
                cwd: std::path::PathBuf::from(cwd),
                pid: pid as u32,
                spawned_at_ms,
            });
        }
        Ok(records)
    }

    pub fn delete_spawn(&self, rowid: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM spawned WHERE id = ?1",
            rusqlite::params![rowid],
        )?;
        Ok(())
    }

    pub fn mark_stopped(&self, backend: BackendKind, session_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO stopped (backend, session_id, stopped_at_ms) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![backend.name(), session_id, crate::spawn::now_ms()],
        )?;
        Ok(())
    }

    pub fn clear_stopped(&self, backend: BackendKind, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM stopped WHERE backend = ?1 AND session_id = ?2",
            rusqlite::params![backend.name(), session_id],
        )?;
        Ok(())
    }

    pub fn set_name_override(
        &self,
        backend: BackendKind,
        session_id: &str,
        name: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO renames (backend, session_id, name) VALUES (?1, ?2, ?3)",
            rusqlite::params![backend.name(), session_id, name],
        )?;
        Ok(())
    }

    /// Drop any name override for this session. Called on a successful native rename so a
    /// stale override (left by an earlier daemon-down fallback) cannot shadow the real name.
    pub fn clear_name_override(&self, backend: BackendKind, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM renames WHERE backend = ?1 AND session_id = ?2",
            rusqlite::params![backend.name(), session_id],
        )?;
        Ok(())
    }

    /// One snapshot read per tick feeding the overlay.
    pub fn viewer_state(&self) -> Result<ViewerState> {
        let mut state = ViewerState::default();

        let mut stmt = self
            .conn
            .prepare("SELECT backend, session_id, pid FROM spawned WHERE session_id IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (backend, session_id, pid) = row?;
            let Some(backend) = backend_from_str(&backend) else {
                continue;
            };
            state.pinned.insert((backend, session_id.clone()));
            state.spawn_pids.insert((backend, session_id), pid as u32);
        }

        let mut stmt = self
            .conn
            .prepare("SELECT backend, session_id FROM stopped")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (backend, session_id) = row?;
            if let Some(backend) = backend_from_str(&backend) {
                state.stopped.insert((backend, session_id));
            }
        }

        let mut stmt = self
            .conn
            .prepare("SELECT backend, session_id, name FROM renames")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (backend, session_id, name) = row?;
            if let Some(backend) = backend_from_str(&backend) {
                state.renames.insert((backend, session_id), name);
            }
        }

        Ok(state)
    }
}

/// PURE overlay, applied in the TUI loop after concatenating backend lists:
///   - renames: replace title
///   - pinned: set companion = false (viewer-spawned sessions always visible)
///   - spawn_pids: fill session.pid when None (opencode stop path)
///   - stopped: Done|Failed -> Stopped. If the session is live again
///     (Working/NeedsInput/Idle) the record is STALE — status unchanged, key
///     returned so the caller clears it from the DB.
///
/// Returns stale stopped keys. Visibility filtering is NOT done here (App owns it).
pub fn apply_viewer_state(
    sessions: &mut [Session],
    state: &ViewerState,
) -> Vec<(BackendKind, String)> {
    let mut stale = Vec::new();
    for session in sessions.iter_mut() {
        let key = (session.backend, session.id.clone());
        if let Some(name) = state.renames.get(&key) {
            session.title = name.clone();
        }
        if state.pinned.contains(&key) {
            session.companion = false;
        }
        if state.stopped.contains(&key) {
            match session.status {
                Status::Done | Status::Failed => session.status = Status::Stopped,
                // Already live again: the stopped record is stale, leave the status.
                Status::Working | Status::NeedsInput | Status::Idle => stale.push(key.clone()),
                Status::Stopped => {}
            }
        }
        // Overlay the spawn pid only onto a still-live session (opencode stop path). A
        // terminal row's recorded pid may have been reused by an unrelated process, so a
        // later stop must never signal it — run this after the stopped fold above so a
        // just-stopped row is excluded too.
        if session.pid.is_none()
            && matches!(
                session.status,
                Status::Working | Status::NeedsInput | Status::Idle
            )
            && let Some(pid) = state.spawn_pids.get(&key)
        {
            session.pid = Some(*pid);
        }
    }
    stale
}

/// PURE spawn matching (runs per tick for unresolved records): a session of
/// record.backend with cwd == record.cwd and created_at_ms within
/// [spawned_at_ms - 2_000, spawned_at_ms + 30_000]; nearest created_at wins on ties.
/// (cwd + time window only — deliberately no pid correlation.)
pub fn match_spawn(record: &SpawnRecord, sessions: &[Session]) -> Option<String> {
    let lo = record.spawned_at_ms - 2_000;
    let hi = record.spawned_at_ms + 30_000;
    let mut best: Option<(&str, i64)> = None;
    for session in sessions {
        if session.backend != record.backend || session.cwd != record.cwd {
            continue;
        }
        if session.created_at_ms < lo || session.created_at_ms > hi {
            continue;
        }
        let distance = (session.created_at_ms - record.spawned_at_ms).abs();
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((session.id.as_str(), distance));
        }
    }
    best.map(|(id, _)| id.to_string())
}
