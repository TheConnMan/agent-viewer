use crate::error::{Error, Result};
use super::source::Source;

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
    pub title: String,
    pub archived: bool,
    pub model: Option<String>,
    pub git_branch: Option<String>,
    pub first_user_message: String,
    pub preview: String,
}

pub struct Registry {
    conn: rusqlite::Connection,
}

impl Registry {
    /// OpenFlags::SQLITE_OPEN_READ_ONLY; busy_timeout 500ms (Codex writes concurrently).
    /// Must NOT create the file if missing (read-only open errors instead).
    pub fn open(db_path: &std::path::Path) -> Result<Registry> {
        Ok(Registry {
            conn: crate::open_readonly(db_path)?,
        })
    }

    /// All rows including archived, recency DESC. COALESCE bridges the nullable *_ms
    /// columns (backfilled by triggers in the live schema) to the *_at * 1000 fallback.
    pub fn threads(&self) -> Result<Vec<Thread>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rollout_path, \
                    COALESCE(created_at_ms, created_at * 1000), \
                    COALESCE(updated_at_ms, updated_at * 1000), \
                    source, cwd, title, archived, model, git_branch, \
                    first_user_message, preview \
             FROM threads \
             ORDER BY COALESCE(updated_at_ms, updated_at * 1000) DESC, id DESC",
        )?;
        let threads = stmt
            .query_map([], |row| {
                Ok(Thread {
                    id: row.get(0)?,
                    rollout_path: std::path::PathBuf::from(row.get::<_, String>(1)?),
                    created_at_ms: row.get(2)?,
                    updated_at_ms: row.get(3)?,
                    source: Source::parse(&row.get::<_, String>(4)?),
                    cwd: std::path::PathBuf::from(row.get::<_, String>(5)?),
                    title: row.get(6)?,
                    archived: row.get::<_, i64>(7)? != 0,
                    model: row.get(8)?,
                    git_branch: row.get(9)?,
                    first_user_message: row.get(10)?,
                    preview: row.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(threads)
    }
}
