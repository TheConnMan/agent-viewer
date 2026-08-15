use super::source::Source;
use crate::error::{Error, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

/// The (mtime, len) freshness key every cache in this crate is keyed by.
pub(crate) type FileKey = (SystemTime, u64);

/// The `session_index.jsonl` overlay, shared so a cache hit copies a pointer, not the map.
type NameOverlay = Arc<HashMap<String, String>>;

/// `path`'s (mtime, len). A missing or unreadable file keys as (UNIX_EPOCH, 0), so one that
/// appears later still invalidates.
pub(crate) fn file_key(path: &std::path::Path) -> FileKey {
    std::fs::metadata(path)
        .map(|meta| {
            (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            )
        })
        .unwrap_or((SystemTime::UNIX_EPOCH, 0))
}

/// Glob <codex_home>/state_*.sqlite, parse the numeric suffix, return the highest N.
/// Numeric compare, not lexicographic (state_10 beats state_5). Err(NoStateDb) if none.
pub fn find_state_db(codex_home: &std::path::Path) -> Result<std::path::PathBuf> {
    let mut best: Option<(u64, std::path::PathBuf)> = None;
    let entries =
        std::fs::read_dir(codex_home).map_err(|_| Error::NoStateDb(codex_home.to_path_buf()))?;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Some(num) = name
            .strip_prefix("state_")
            .and_then(|rest| rest.strip_suffix(".sqlite"))
        else {
            continue;
        };
        let Ok(n) = num.parse::<u64>() else { continue };
        if best.as_ref().is_none_or(|(b, _)| n > *b) {
            best = Some((n, entry.path()));
        }
    }
    best.map(|(_, path)| path)
        .ok_or_else(|| Error::NoStateDb(codex_home.to_path_buf()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Thread {
    pub id: String,
    pub rollout_path: std::path::PathBuf,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub source: super::source::Source,
    pub parent_thread_id: Option<String>,
    pub cwd: std::path::PathBuf,
    pub git_branch: Option<String>,
    pub title: String,
    pub archived: bool,
    pub preview: String,
}

pub struct Registry {
    conn: rusqlite::Connection,
    session_index_path: std::path::PathBuf,
    /// The parsed `session_index.jsonl` name overlay, keyed by its (mtime, len) exactly like
    /// the tail and job-state caches. Without it the whole file is re-read and re-parsed on
    /// every listing tick even though it changes only when a thread is renamed.
    names: RefCell<Option<(FileKey, NameOverlay)>>,
}

impl Registry {
    /// OpenFlags::SQLITE_OPEN_READ_ONLY; busy_timeout 500ms (Codex writes concurrently).
    /// Must NOT create the file if missing (read-only open errors instead).
    pub fn open(db_path: &std::path::Path) -> Result<Registry> {
        Ok(Registry {
            conn: crate::open_readonly(db_path)?,
            session_index_path: db_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(""))
                .join("session_index.jsonl"),
            names: RefCell::new(None),
        })
    }

    /// The `session_index.jsonl` name overlay, re-read only when the file's (mtime, len)
    /// changes. Shared behind an `Arc` so a cache hit copies a pointer, not the map.
    fn thread_names(&self) -> NameOverlay {
        let key = file_key(&self.session_index_path);
        let mut cached = self.names.borrow_mut();
        if let Some((cached_key, names)) = cached.as_ref()
            && *cached_key == key
        {
            return Arc::clone(names);
        }
        let names = Arc::new(read_thread_names(&self.session_index_path));
        *cached = Some((key, Arc::clone(&names)));
        names
    }

    /// All rows including archived, ordered by effective updated_at_ms ascending and id
    /// descending. COALESCE bridges the nullable *_ms columns to the *_at * 1000 fallback.
    pub fn threads(&self) -> Result<Vec<Thread>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rollout_path, \
                    COALESCE(created_at_ms, created_at * 1000), \
                    COALESCE(updated_at_ms, updated_at * 1000), \
                    source, cwd, git_branch, title, archived, preview \
             FROM threads",
        )?;
        let mut threads = stmt
            .query_map([], |row| {
                let raw_source = row.get::<_, String>(4)?;
                let source = Source::parse(&raw_source);
                // Only a subagent row can carry a spawn parent: every other `source` is one of
                // the flat `cli`/`exec`/`vscode` strings, which `Source::parse` matches before
                // it ever reaches JSON. Gating here keeps the JSON parse to once per row
                // instead of once in each of these two fields.
                let parent_thread_id = matches!(source, Source::Subagent(_))
                    .then(|| spawn_parent_thread_id(&raw_source))
                    .flatten();
                Ok(Thread {
                    id: row.get(0)?,
                    rollout_path: std::path::PathBuf::from(row.get::<_, String>(1)?),
                    created_at_ms: row.get(2)?,
                    updated_at_ms: row.get(3)?,
                    source,
                    parent_thread_id,
                    cwd: std::path::PathBuf::from(row.get::<_, String>(5)?),
                    git_branch: row.get(6)?,
                    title: row.get(7)?,
                    archived: row.get::<_, i64>(8)? != 0,
                    preview: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        threads.sort_by(|a, b| {
            a.updated_at_ms
                .cmp(&b.updated_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        let names = self.thread_names();
        for thread in &mut threads {
            if let Some(name) = names.get(&thread.id) {
                thread.title.clone_from(name);
            }
        }
        Ok(threads)
    }

    /// Distinct non-empty `model` values across all threads, most-used first (bare slugs,
    /// no provider prefix). Best-effort discovery source for the model picker fallback.
    pub fn distinct_models(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT model FROM threads WHERE model IS NOT NULL AND model != '' \
             GROUP BY model ORDER BY COUNT(*) DESC",
        )?;
        let models = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(models)
    }
}

fn spawn_parent_thread_id(raw_source: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw_source)
        .ok()?
        .get("subagent")?
        .get("thread_spawn")?
        .get("parent_thread_id")?
        .as_str()
        .map(str::trim)
        .filter(|parent| !parent.is_empty())
        .map(str::to_string)
}

fn read_thread_names(path: &std::path::Path) -> HashMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut names = HashMap::new();
    for line in contents.lines() {
        let Some(value) = crate::parse_json_line(line) else {
            continue;
        };
        let Some(id) = crate::json_str(&value, "id") else {
            continue;
        };
        let Some(name) = crate::json_str(&value, "thread_name") else {
            continue;
        };
        if id.trim().is_empty() || name.trim().is_empty() {
            continue;
        }
        names.insert(id.to_string(), name.to_string());
    }
    names
}
