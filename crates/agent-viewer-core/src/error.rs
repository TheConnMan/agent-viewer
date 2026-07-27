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
    /// Should the caller fall back to the read-only live tail (the inline peek) instead of
    /// just printing `reason`? True when the session IS running and watchable but cannot be
    /// joined, which is the whole point: the user asked to see it, and a transcript that
    /// updates as it runs is the honest answer. False when there is nothing to watch (a
    /// backend that does not support attach at all).
    pub tail: bool,
}

impl AttachRefusal {
    pub fn new(reason: impl Into<String>) -> AttachRefusal {
        AttachRefusal {
            reason: reason.into(),
            tail: false,
        }
    }

    /// A refusal whose row should fall back to the read-only live tail.
    pub fn tailable(reason: impl Into<String>) -> AttachRefusal {
        AttachRefusal {
            reason: reason.into(),
            tail: true,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
