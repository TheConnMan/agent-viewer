//! Viewer-owned state DB (the ONLY SQLite this tool writes) plus the pure overlay and
//! spawn-matching functions. Backends stay ignorant of it: they never read or write this
//! file, so nothing here can alter what a backend reports. The overlay is presentation-only
//! now - it unhides viewer-spawned sessions (clearing `companion`) and fills in the pid the
//! viewer recorded at spawn time for rows the backend could not supply one for. Titles,
//! statuses, and hidden flags come from the backend unchanged.

use crate::backend::{BackendKind, Session};
use crate::error::Result;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS spawned (\
  id INTEGER PRIMARY KEY,\
  backend TEXT NOT NULL,\
  session_id TEXT,\
  cwd TEXT NOT NULL,\
  pid INTEGER NOT NULL,\
  spawned_at_ms INTEGER NOT NULL\
);\
CREATE TABLE IF NOT EXISTS collapsed_groups (group_key TEXT PRIMARY KEY);\
CREATE TABLE IF NOT EXISTS settings (\
  key TEXT PRIMARY KEY,\
  value TEXT NOT NULL\
);\
CREATE TABLE IF NOT EXISTS model_cache (\
  backend TEXT PRIMARY KEY,\
  models TEXT NOT NULL,\
  fetched_at_ms INTEGER NOT NULL\
);";

/// Legacy shadow-state tables the viewer no longer owns. Dropped once after a successful
/// open, NOT as part of `SCHEMA`: a DROP takes a write lock and can return SQLITE_BUSY under
/// the WAL multi-viewer concurrency this file designs for, and a failure inside `try_open`
/// would send `open` down the delete-and-recreate path, costing the user their spawn pins and
/// collapsed groups over a transient lock.
const DROP_LEGACY_TABLES: &str = "\
DROP TABLE IF EXISTS renames;\
DROP TABLE IF EXISTS stopped;";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupingMode {
    #[default]
    Project,
    State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Recency,
    Title,
}

pub const DEFAULT_RETENTION_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A backend's stored model catalog and the epoch-ms it was fetched at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedModels {
    /// Model ids in picker order (the backend's default first).
    pub models: Vec<String>,
    pub fetched_at_ms: i64,
}

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

/// Does this open failure mean the FILE ITSELF is unusable, so that a fresh empty DB is
/// strictly better than what is on disk? Only two sqlite codes say that: the bytes are not a
/// database at all, or the database is corrupt. Everything else - `DatabaseBusy` and
/// `DatabaseLocked` most of all, but equally a permissions or disk error - describes the
/// environment, not the file, and the file must survive it.
fn is_unusable_file(error: &crate::error::Error) -> bool {
    let crate::error::Error::Sqlite(rusqlite::Error::SqliteFailure(failure, _)) = error else {
        return false;
    };
    matches!(
        failure.code,
        rusqlite::ffi::ErrorCode::NotADatabase | rusqlite::ffi::ErrorCode::DatabaseCorrupt
    )
}

fn create_state_parent(path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current);
        current = current.parent().unwrap_or(current);
    }

    for directory in missing.into_iter().rev() {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn restrict_database_files(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_os_string();
        file.push(suffix);
        let file = std::path::PathBuf::from(file);
        if file.exists() {
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
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
    /// Resolved spawns that remain visible.
    pub pinned: HashSet<(BackendKind, String)>,
    pub spawn_pids: HashMap<(BackendKind, String), u32>,
}

impl ViewerDb {
    /// $HOME/.local/state/agent-viewer/viewer.db, parent dir created. Read-WRITE.
    /// A file that is not a database, or is corrupt, is deleted and recreated fresh (its
    /// contents are advisory; losing them costs pins and collapsed groups, nothing more).
    /// EVERY other open failure - lock contention above all - is returned to the caller with
    /// the file left untouched.
    pub fn open_default() -> Result<ViewerDb> {
        ViewerDb::open(&crate::home_dir().join(".local/state/agent-viewer/viewer.db"))
    }

    /// tests: temp path.
    pub fn open(path: &std::path::Path) -> Result<ViewerDb> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            create_state_parent(parent)?;
        }
        let db = match ViewerDb::try_open(path) {
            Ok(db) => db,
            // Delete-and-recreate ONLY when the file itself is garbage, never on contention.
            // `SCHEMA` contains CREATE TABLE statements, and adding a table to an existing
            // viewer.db is a real write that takes a write lock; under the WAL multi-viewer
            // concurrency this file is designed for, a second viewer can lose that race and
            // get SQLITE_BUSY after the 500ms busy_timeout. A busy loser must never destroy
            // the winner's live database - that would wipe the running viewer's spawn pins
            // and collapsed groups mid-session. Anything else (permissions, disk, an unmapped
            // sqlite code) also errors out rather than deleting: an unreadable-for-some-other-
            // reason database is not made better by being destroyed, and the failure is
            // usually transient or environmental.
            Err(error) if is_unusable_file(&error) => {
                let _ = std::fs::remove_file(path);
                // The WAL sidecars belong to the file we just removed; a stale -wal replayed
                // over the fresh database would recreate the corruption we are recovering from.
                for suffix in ["-wal", "-shm"] {
                    let mut sidecar = path.as_os_str().to_os_string();
                    sidecar.push(suffix);
                    let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
                }
                ViewerDb::try_open(path)?
            }
            Err(error) => return Err(error),
        };
        // Best effort: a lock contention failure here leaves the legacy tables in place for
        // the next open rather than discarding a usable DB.
        let _ = db.conn.execute_batch(DROP_LEGACY_TABLES);
        restrict_database_files(path)?;
        Ok(db)
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

    /// Persist a group's collapsed state: insert its key when collapsed, delete it when
    /// expanded. The key is an opaque text form owned by the TUI's GroupKey.
    pub fn set_group_collapsed(&self, group_key: &str, collapsed: bool) -> Result<()> {
        if collapsed {
            self.conn.execute(
                "INSERT OR REPLACE INTO collapsed_groups (group_key) VALUES (?1)",
                rusqlite::params![group_key],
            )?;
        } else {
            self.conn.execute(
                "DELETE FROM collapsed_groups WHERE group_key = ?1",
                rusqlite::params![group_key],
            )?;
        }
        Ok(())
    }

    /// Every collapsed-group key (opaque strings the TUI seeds its collapsed set from).
    pub fn collapsed_groups(&self) -> Result<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT group_key FROM collapsed_groups")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut keys = HashSet::new();
        for row in rows {
            keys.insert(row?);
        }
        Ok(keys)
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        let value = self.conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get(0),
        );
        match value {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    pub fn grouping_mode(&self) -> Result<GroupingMode> {
        Ok(match self.setting("grouping_mode")?.as_deref() {
            Some("state") => GroupingMode::State,
            Some("project") | None => GroupingMode::Project,
            Some(_) => GroupingMode::default(),
        })
    }

    pub fn set_grouping_mode(&self, mode: GroupingMode) -> Result<()> {
        let value = match mode {
            GroupingMode::Project => "project",
            GroupingMode::State => "state",
        };
        self.set_setting("grouping_mode", value)
    }

    pub fn sort_order(&self) -> Result<SortOrder> {
        Ok(match self.setting("sort_order")?.as_deref() {
            Some("title") => SortOrder::Title,
            Some("recency") | None => SortOrder::Recency,
            Some(_) => SortOrder::default(),
        })
    }

    pub fn set_sort_order(&self, order: SortOrder) -> Result<()> {
        let value = match order {
            SortOrder::Recency => "recency",
            SortOrder::Title => "title",
        };
        self.set_setting("sort_order", value)
    }

    pub fn retention_window_ms(&self) -> Result<i64> {
        Ok(self
            .setting("retention_window_ms")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_RETENTION_WINDOW_MS))
    }

    pub fn set_retention_window_ms(&self, ms: i64) -> Result<()> {
        self.set_setting("retention_window_ms", &ms.to_string())
    }

    /// Store a backend's discovered model catalog, stamped with the time it was fetched.
    /// Ids are newline-joined: `available_models` builds them from single lines of CLI
    /// output, so no id can contain the separator.
    pub fn set_cached_models(
        &self,
        backend: BackendKind,
        models: &[String],
        fetched_at_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO model_cache (backend, models, fetched_at_ms) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![backend.name(), models.join("\n"), fetched_at_ms],
        )?;
        Ok(())
    }

    /// A backend's stored catalog, or None when it has never been discovered. Whether the
    /// row is too old to trust is the caller's policy, not this file's - the fetch stamp is
    /// returned unjudged.
    pub fn cached_models(&self, backend: BackendKind) -> Result<Option<CachedModels>> {
        let row = self.conn.query_row(
            "SELECT models, fetched_at_ms FROM model_cache WHERE backend = ?1",
            rusqlite::params![backend.name()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        );
        match row {
            Ok((models, fetched_at_ms)) => Ok(Some(CachedModels {
                models: models
                    .split('\n')
                    .filter(|id| !id.is_empty())
                    .map(|id| id.to_string())
                    .collect(),
                fetched_at_ms,
            })),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn opencode_server_url(&self) -> Result<Option<String>> {
        self.setting("opencode.server_url")
    }

    pub fn set_opencode_server_url(&self, url: Option<&str>) -> Result<()> {
        if let Some(url) = url {
            self.set_setting("opencode.server_url", url)
        } else {
            self.conn.execute(
                "DELETE FROM settings WHERE key = ?1",
                rusqlite::params!["opencode.server_url"],
            )?;
            Ok(())
        }
    }

    pub fn opencode_server_secret(&self) -> Result<String> {
        use base64::Engine;
        use std::io::Read;

        let transaction = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let existing = transaction.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            rusqlite::params!["opencode.server_password"],
            |row| row.get::<_, String>(0),
        );
        match existing {
            Ok(secret) => {
                transaction.commit()?;
                Ok(secret)
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                let mut random = [0u8; 32];
                std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
                let candidate = base64::engine::general_purpose::STANDARD.encode(random);
                transaction.execute(
                    "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
                    rusqlite::params!["opencode.server_password", candidate],
                )?;
                let secret = transaction.query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    rusqlite::params!["opencode.server_password"],
                    |row| row.get::<_, String>(0),
                )?;
                transaction.commit()?;
                Ok(secret)
            }
            Err(error) => Err(error.into()),
        }
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

        Ok(state)
    }
}

/// Pure presentation overlay for sessions created by the viewer.
pub fn apply_viewer_state(sessions: &mut [Session], state: &ViewerState) {
    for session in sessions.iter_mut() {
        let key = (session.backend, session.id.clone());
        if state.pinned.contains(&key) {
            session.companion = false;
        }
        // A terminal row's recorded pid may already have been recycled by an unrelated
        // process, so never hand it out: a later stop would signal the wrong process.
        //
        // A daemon-hosted row is skipped outright. Its pid is None BY DESIGN (the fd holder is
        // the app-server, whose pid belongs to every other thread it hosts), and "no pid, not
        // finished" is that row's permanent shape - exactly the hole this overlay would fill,
        // putting a signalable pid back on the one row that must never be signalled.
        if session.pid.is_none()
            && !session.daemon_hosted
            && !session.status.is_finished()
            && let Some(pid) = state.spawn_pids.get(&key)
        {
            session.pid = Some(*pid);
        }
    }
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
