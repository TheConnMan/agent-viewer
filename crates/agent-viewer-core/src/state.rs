//! Viewer-owned state DB (the ONLY SQLite this tool writes) plus the pure overlay and
//! spawn-matching functions. Backends stay ignorant of it: they never read or write this
//! file, so nothing here can alter what a backend reports. The overlay is presentation-only
//! now - it unhides viewer-spawned sessions (clearing `companion`) and fills in the pid the
//! viewer recorded at spawn time for rows the backend could not supply one for. Titles,
//! statuses, and hidden flags come from the backend unchanged.

use crate::backend::{BackendKind, ListingCacheScope, Session};
use crate::error::Result;
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

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
);\
CREATE TABLE IF NOT EXISTS backend_listing_cache (\
  backend TEXT NOT NULL,\
  scope TEXT NOT NULL,\
  snapshot_json TEXT,\
  refreshed_at_ms INTEGER NOT NULL DEFAULT 0,\
  generation INTEGER NOT NULL DEFAULT 0,\
  lease_owner TEXT,\
  lease_until_ms INTEGER NOT NULL DEFAULT 0,\
  PRIMARY KEY (backend, scope)\
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

pub const LISTING_CACHE_FRESHNESS_MS: i64 = 2_000;
pub const LISTING_CACHE_LEASE_MS: i64 = 5_000;
const VIEWER_DB_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// A decoded raw backend listing plus the publication time stored by the coordinator.
#[derive(Debug, Clone, PartialEq)]
pub struct ListingCacheSnapshot {
    sessions: Vec<Session>,
    snapshot_json: Option<String>,
    published_at_ms: i64,
    published_generation: i64,
}

impl ListingCacheSnapshot {
    pub fn from_sessions(sessions: Vec<Session>) -> Result<ListingCacheSnapshot> {
        let snapshot_json = serde_json::to_string(&sessions)?;
        Ok(ListingCacheSnapshot {
            sessions,
            snapshot_json: Some(snapshot_json),
            published_at_ms: 0,
            published_generation: 0,
        })
    }

    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn into_sessions(self) -> Vec<Session> {
        self.sessions
    }

    pub const fn published_at_ms(&self) -> i64 {
        self.published_at_ms
    }

    pub const fn published_generation(&self) -> i64 {
        self.published_generation
    }

    fn decode(
        scope: &ListingCacheScope,
        snapshot_json: &str,
        published_at_ms: i64,
        published_generation: i64,
    ) -> Option<ListingCacheSnapshot> {
        let sessions = serde_json::from_str::<Vec<Session>>(snapshot_json).ok()?;
        if sessions
            .iter()
            .any(|session| session.backend != scope.backend())
        {
            return None;
        }
        Some(ListingCacheSnapshot {
            sessions,
            snapshot_json: None,
            published_at_ms,
            published_generation,
        })
    }
}

/// A generation fenced right to perform one authoritative backend listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingCacheLease {
    scope: ListingCacheScope,
    owner: String,
    generation: i64,
}

impl ListingCacheLease {
    pub const fn next_published_generation(&self) -> i64 {
        self.generation.saturating_add(1)
    }
}

pub struct ListingRefreshGuard {
    stop: std::sync::mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ListingRefreshGuard {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ListingCacheRead {
    Fresh(ListingCacheSnapshot),
    Stale(ListingCacheSnapshot),
    Miss,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ListingCacheClaim {
    Bypass,
    Fresh(ListingCacheSnapshot),
    Unchanged,
    LeaseHeld,
    Claimed(ListingCacheLease),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingCacheWrite {
    Published,
    Fenced,
}

struct ListingCacheRow {
    refreshed_at_ms: i64,
    generation: i64,
    lease_owner: Option<String>,
    lease_until_ms: i64,
}

fn listing_cache_row(
    connection: &rusqlite::Connection,
    scope: &ListingCacheScope,
) -> rusqlite::Result<Option<ListingCacheRow>> {
    let row = connection.query_row(
        "SELECT refreshed_at_ms, generation, lease_owner, lease_until_ms \
         FROM backend_listing_cache WHERE backend = ?1 AND scope = ?2",
        rusqlite::params![scope.backend().name(), scope.as_str()],
        |row| {
            Ok(ListingCacheRow {
                refreshed_at_ms: row.get(0)?,
                generation: row.get(1)?,
                lease_owner: row.get(2)?,
                lease_until_ms: row.get(3)?,
            })
        },
    );
    match row {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error),
    }
}

fn listing_snapshot(
    connection: &rusqlite::Connection,
    scope: &ListingCacheScope,
    row: &ListingCacheRow,
) -> rusqlite::Result<Option<ListingCacheSnapshot>> {
    connection.query_row(
        "SELECT snapshot_json FROM backend_listing_cache WHERE backend = ?1 AND scope = ?2",
        rusqlite::params![scope.backend().name(), scope.as_str()],
        |sqlite_row| {
            let rusqlite::types::ValueRef::Text(snapshot_json) = sqlite_row.get_ref(0)? else {
                return Ok(None);
            };
            Ok(std::str::from_utf8(snapshot_json)
                .ok()
                .and_then(|snapshot_json| {
                    ListingCacheSnapshot::decode(
                        scope,
                        snapshot_json,
                        row.refreshed_at_ms,
                        row.generation,
                    )
                }))
        },
    )
}

fn listing_snapshot_is_fresh(refreshed_at_ms: i64, now_ms: i64, freshness_ms: i64) -> bool {
    refreshed_at_ms > 0 && freshness_ms > 0 && now_ms.saturating_sub(refreshed_at_ms) < freshness_ms
}

fn listing_lease_owner(now_ms: i64) -> String {
    static NEXT_OWNER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sequence = NEXT_OWNER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}:{now_ms}:{sequence}", std::process::id())
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
    path: PathBuf,
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

#[cfg(unix)]
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

#[cfg(not(unix))]
fn create_state_parent(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

#[cfg(unix)]
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

#[cfg(not(unix))]
fn restrict_database_files(_path: &Path) -> Result<()> {
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
        conn.busy_timeout(VIEWER_DB_BUSY_TIMEOUT)?;
        // WAL so two concurrent viewers coexist (last-writer-wins on advisory state).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(ViewerDb {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// Read a display snapshot without changing its freshness or lease.
    pub fn read_listing_snapshot(
        &self,
        scope: &ListingCacheScope,
        now_ms: i64,
    ) -> Result<ListingCacheRead> {
        let Some(row) = listing_cache_row(&self.conn, scope)? else {
            return Ok(ListingCacheRead::Miss);
        };
        let Some(snapshot) = listing_snapshot(&self.conn, scope, &row)? else {
            return Ok(ListingCacheRead::Miss);
        };
        if listing_snapshot_is_fresh(row.refreshed_at_ms, now_ms, LISTING_CACHE_FRESHNESS_MS) {
            Ok(ListingCacheRead::Fresh(snapshot))
        } else {
            Ok(ListingCacheRead::Stale(snapshot))
        }
    }

    /// Atomically reread one scope and either reuse it, observe its live lease, or claim it.
    pub fn claim_listing_refresh(
        &self,
        scope: Option<&ListingCacheScope>,
        known_generation: Option<i64>,
        now_ms: i64,
        freshness_ms: i64,
        lease_ms: i64,
    ) -> Result<ListingCacheClaim> {
        let Some(scope) = scope else {
            return Ok(ListingCacheClaim::Bypass);
        };
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let row = listing_cache_row(&transaction, scope)?;
        if let Some(row) = row.as_ref() {
            if listing_snapshot_is_fresh(row.refreshed_at_ms, now_ms, freshness_ms) {
                if known_generation == Some(row.generation) {
                    transaction.commit()?;
                    return Ok(ListingCacheClaim::Unchanged);
                }
                if let Some(snapshot) = listing_snapshot(&transaction, scope, row)? {
                    transaction.commit()?;
                    return Ok(ListingCacheClaim::Fresh(snapshot));
                }
            }
            if row.lease_owner.is_some() && row.lease_until_ms > now_ms {
                if known_generation == Some(row.generation) {
                    transaction.commit()?;
                    return Ok(ListingCacheClaim::Unchanged);
                }
                transaction.commit()?;
                return Ok(ListingCacheClaim::LeaseHeld);
            }
        }

        let owner = listing_lease_owner(now_ms);
        let generation = row.as_ref().map_or(0, |row| row.generation);
        let lease_until_ms = now_ms.saturating_add(lease_ms.max(0));
        if row.is_some() {
            transaction.execute(
                "UPDATE backend_listing_cache \
                 SET lease_owner = ?1, lease_until_ms = ?2 \
                 WHERE backend = ?3 AND scope = ?4",
                rusqlite::params![
                    owner,
                    lease_until_ms,
                    scope.backend().name(),
                    scope.as_str()
                ],
            )?;
        } else {
            transaction.execute(
                "INSERT INTO backend_listing_cache \
                 (backend, scope, snapshot_json, refreshed_at_ms, generation, lease_owner, lease_until_ms) \
                 VALUES (?1, ?2, NULL, 0, ?3, ?4, ?5)",
                rusqlite::params![
                    scope.backend().name(),
                    scope.as_str(),
                    generation,
                    owner,
                    lease_until_ms
                ],
            )?;
        }
        transaction.commit()?;
        Ok(ListingCacheClaim::Claimed(ListingCacheLease {
            scope: scope.clone(),
            owner,
            generation,
        }))
    }

    /// Keep an owned listing lease alive while its authoritative source is still running.
    pub fn guard_listing_refresh(
        &self,
        lease: &ListingCacheLease,
        lease_ms: i64,
    ) -> Result<ListingRefreshGuard> {
        let connection = rusqlite::Connection::open(&self.path)?;
        connection.busy_timeout(VIEWER_DB_BUSY_TIMEOUT)?;
        let scope = lease.scope.clone();
        let owner = lease.owner.clone();
        let generation = lease.generation;
        let lease_ms = lease_ms.max(1);
        let renewal_interval = std::time::Duration::from_millis((lease_ms / 3).max(1) as u64);
        let (stop, stopped) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("listing_lease_renewal".to_string())
            .spawn(move || {
                loop {
                    match stopped.recv_timeout(renewal_interval) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let lease_until_ms = crate::spawn::now_ms().saturating_add(lease_ms);
                    match connection.execute(
                        "UPDATE backend_listing_cache SET lease_until_ms = ?1 \
                         WHERE backend = ?2 AND scope = ?3 AND generation = ?4 \
                         AND lease_owner = ?5",
                        rusqlite::params![
                            lease_until_ms,
                            scope.backend().name(),
                            scope.as_str(),
                            generation,
                            owner
                        ],
                    ) {
                        Ok(1) => {}
                        Ok(_) => break,
                        Err(_) => {}
                    }
                }
            })?;
        Ok(ListingRefreshGuard {
            stop,
            worker: Some(worker),
        })
    }

    /// Publish only if this caller still owns the generation it claimed.
    pub fn publish_listing(
        &self,
        lease: &ListingCacheLease,
        snapshot: ListingCacheSnapshot,
        now_ms: i64,
    ) -> Result<ListingCacheWrite> {
        let Some(snapshot_json) = snapshot.snapshot_json else {
            return Err(crate::error::Error::Command(
                "decoded listing snapshot cannot be published".to_string(),
            ));
        };
        let changed = self.conn.execute(
            "UPDATE backend_listing_cache \
             SET snapshot_json = ?1, refreshed_at_ms = ?2, generation = ?3, \
                 lease_owner = NULL, lease_until_ms = 0 \
             WHERE backend = ?4 AND scope = ?5 AND generation = ?6 AND lease_owner = ?7",
            rusqlite::params![
                snapshot_json,
                now_ms,
                lease.next_published_generation(),
                lease.scope.backend().name(),
                lease.scope.as_str(),
                lease.generation,
                lease.owner
            ],
        )?;
        Ok(if changed == 1 {
            ListingCacheWrite::Published
        } else {
            ListingCacheWrite::Fenced
        })
    }

    /// Release a failed authoritative refresh without changing the last good snapshot.
    pub fn fail_listing_refresh(&self, lease: &ListingCacheLease) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE backend_listing_cache SET lease_owner = NULL, lease_until_ms = 0 \
             WHERE backend = ?1 AND scope = ?2 AND generation = ?3 AND lease_owner = ?4",
            rusqlite::params![
                lease.scope.backend().name(),
                lease.scope.as_str(),
                lease.generation,
                lease.owner
            ],
        )?;
        Ok(changed == 1)
    }

    /// Fence an in flight writer and make the current display snapshot explicitly stale.
    pub fn invalidate_listing_scope(&self, scope: Option<&ListingCacheScope>) -> Result<bool> {
        let Some(scope) = scope else {
            return Ok(false);
        };
        let changed = self.conn.execute(
            "UPDATE backend_listing_cache \
             SET refreshed_at_ms = 0, generation = generation + 1, \
                 lease_owner = NULL, lease_until_ms = 0 \
             WHERE backend = ?1 AND scope = ?2",
            rusqlite::params![scope.backend().name(), scope.as_str()],
        )?;
        Ok(changed == 1)
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

    /// The header mascot, stored by name. The viewer owns the set of valid names; an unknown one
    /// simply falls back to its default, so a downgrade cannot wedge the header.
    pub fn header_sprite(&self) -> Result<Option<String>> {
        self.setting("header_sprite")
    }

    pub fn set_header_sprite(&self, name: &str) -> Result<()> {
        self.set_setting("header_sprite", name)
    }

    /// Whether finished rows fade toward the theme's faint token as they age. Unset reads as
    /// off: the mode is opt-in, so a viewer that has never been told otherwise never ramps.
    pub fn age_ramp(&self) -> Result<bool> {
        Ok(self.setting("age_ramp")?.as_deref() == Some("1"))
    }

    pub fn set_age_ramp(&self, enabled: bool) -> Result<()> {
        self.set_setting("age_ramp", if enabled { "1" } else { "0" })
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
    match_spawn_between(
        record.backend,
        &record.cwd,
        record.spawned_at_ms,
        record.spawned_at_ms,
        sessions,
    )
}

/// PURE spawn matching over the INTERVAL the session could have been created in: a session of
/// `backend` with cwd == `cwd` and created_at_ms within
/// [earliest_ms - 2_000, latest_ms + 30_000]; the candidate nearest that interval wins.
///
/// A direct spawn knows the one instant it happened, so it passes the same value twice and gets
/// the single-stamp window above. A ROUTED spawn does not: the router classifies, dispatches, and
/// then polls for the winning backend's job id, so the job is created somewhere between the
/// invocation (`earliest_ms`) and the return (`latest_ms`) and can be many seconds older than the
/// decision the viewer stamps. Bracketing the whole call is what keeps that row selectable.
pub fn match_spawn_between(
    backend: BackendKind,
    cwd: &Path,
    earliest_ms: i64,
    latest_ms: i64,
    sessions: &[Session],
) -> Option<String> {
    let (earliest_ms, latest_ms) = (earliest_ms.min(latest_ms), earliest_ms.max(latest_ms));
    let lo = earliest_ms.saturating_sub(2_000);
    let hi = latest_ms.saturating_add(30_000);
    let mut best: Option<(&str, u64)> = None;
    for session in sessions {
        if session.backend != backend || session.cwd != cwd {
            continue;
        }
        if session.created_at_ms < lo || session.created_at_ms > hi {
            continue;
        }
        // Distance to the interval, not to a point: every row created while the router ran is
        // equally plausible, and rows outside rank by how far out they sit. Identical to
        // |created_at - spawned_at| when the interval is a single instant.
        let clamped = session.created_at_ms.clamp(earliest_ms, latest_ms);
        let distance = session.created_at_ms.abs_diff(clamped);
        if best.is_none_or(|(_, best_distance)| distance < best_distance) {
            best = Some((session.id.as_str(), distance));
        }
    }
    best.map(|(id, _)| id.to_string())
}
