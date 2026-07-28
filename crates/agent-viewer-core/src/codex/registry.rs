use super::source::Source;
use crate::error::{Error, Result};

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
    pub cwd: std::path::PathBuf,
    pub git_branch: Option<String>,
    pub title: String,
    pub archived: bool,
    pub preview: String,
}

pub struct Registry {
    conn: rusqlite::Connection,
    session_index_path: std::path::PathBuf,
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
        })
    }

    /// All rows including archived, updated_at_ms ascending. COALESCE bridges the nullable *_ms
    /// columns (backfilled by triggers in the live schema) to the *_at * 1000 fallback.
    pub fn threads(&self) -> Result<Vec<Thread>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rollout_path, \
                    COALESCE(created_at_ms, created_at * 1000), \
                    COALESCE(updated_at_ms, updated_at * 1000), \
                    source, cwd, git_branch, title, archived, preview \
             FROM threads \
             ORDER BY COALESCE(updated_at_ms, updated_at * 1000) ASC, id DESC",
        )?;
        let mut threads = stmt
            .query_map([], |row| {
                Ok(Thread {
                    id: row.get(0)?,
                    rollout_path: std::path::PathBuf::from(row.get::<_, String>(1)?),
                    created_at_ms: row.get(2)?,
                    updated_at_ms: row.get(3)?,
                    source: Source::parse(&row.get::<_, String>(4)?),
                    cwd: std::path::PathBuf::from(row.get::<_, String>(5)?),
                    git_branch: row.get(6)?,
                    title: row.get(7)?,
                    archived: row.get::<_, i64>(8)? != 0,
                    preview: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let names = read_thread_names(&self.session_index_path);
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

fn read_thread_names(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return std::collections::HashMap::new();
    };
    let mut names = std::collections::HashMap::new();
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
