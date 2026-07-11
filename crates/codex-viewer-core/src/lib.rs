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
    todo!()
}
