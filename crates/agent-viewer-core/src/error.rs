#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no state_*.sqlite found under {0}")]
    NoStateDb(std::path::PathBuf),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("command failed: {0}")]
    Command(String),
    #[error("{0} does not support this action")]
    Unsupported(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct AttachRefusal {
    pub reason: String,
}

impl AttachRefusal {
    pub fn new(reason: impl Into<String>) -> AttachRefusal {
        AttachRefusal {
            reason: reason.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
