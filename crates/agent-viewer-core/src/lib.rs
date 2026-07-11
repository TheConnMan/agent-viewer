pub mod backend;
pub mod claude;
pub mod codex;
pub mod error;
pub mod group;
pub mod opencode;
pub mod spawn;

pub use backend::{Backend, BackendKind, Capabilities, Session, Status};
pub use error::{Error, Result};

/// Open a SQLite DB read-only with a 500ms busy timeout (Codex and opencode write
/// concurrently). Read-only flags mean the file is never created if missing.
pub fn open_readonly(path: &std::path::Path) -> Result<rusqlite::Connection> {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_millis(500))?;
    Ok(conn)
}

/// $CODEX_HOME if set, else $HOME/.codex.
pub fn default_codex_home() -> std::path::PathBuf {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        return std::path::PathBuf::from(codex_home);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".codex")
}
