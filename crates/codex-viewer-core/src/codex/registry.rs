use crate::error::Result;

/// Glob <codex_home>/state_*.sqlite, parse the numeric suffix, return the highest N.
/// Numeric compare, not lexicographic (state_10 beats state_5). Err(NoStateDb) if none.
pub fn find_state_db(codex_home: &std::path::Path) -> Result<std::path::PathBuf> {
    let _ = codex_home;
    todo!()
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
    _conn: rusqlite::Connection,
}

impl Registry {
    /// OpenFlags::SQLITE_OPEN_READ_ONLY; busy_timeout 500ms (Codex writes concurrently).
    /// Must NOT create the file if missing (read-only open errors instead).
    pub fn open(db_path: &std::path::Path) -> Result<Registry> {
        let _ = db_path;
        todo!()
    }
    /// All rows including archived, recency DESC.
    pub fn threads(&self) -> Result<Vec<Thread>> {
        todo!()
    }
}
