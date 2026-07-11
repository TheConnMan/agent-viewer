pub mod backend;
pub mod claude;
pub mod codex;
pub mod error;
pub mod group;
pub mod opencode;
pub mod pty;
pub mod spawn;
pub mod state;

pub use backend::{Backend, BackendKind, Capabilities, Session, Status};
pub use error::{Error, Result};

/// Flag any session whose cwd is a non-empty path that no longer exists on disk as a
/// companion, so the default view hides deleted-dir noise (e.g. agentos /tmp sessions).
/// Only ever SETS companion — an already-flagged session and a session with a live or
/// empty cwd are left untouched.
pub fn mark_dead_dirs(sessions: &mut [Session]) {
    for session in sessions.iter_mut() {
        if session.companion {
            continue;
        }
        if session.cwd.as_os_str().is_empty() {
            continue;
        }
        if !session.cwd.exists() {
            session.companion = true;
        }
    }
}

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
