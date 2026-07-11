pub mod backend;
pub mod claude;
pub mod codex;
pub mod error;
pub mod group;
pub mod opencode;
pub mod spawn;

pub use backend::{Backend, BackendKind, Capabilities, Session, Status};
pub use error::{Error, Result};

/// $CODEX_HOME if set, else $HOME/.codex.
pub fn default_codex_home() -> std::path::PathBuf {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        return std::path::PathBuf::from(codex_home);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".codex")
}
