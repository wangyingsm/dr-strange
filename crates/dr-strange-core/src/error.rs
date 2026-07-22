//! Core error taxonomy (arch/04 §6). Single enum for now — revisit at M0 close.

/// All fallible core operations return this error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),

    /// E.g. an edge whose endpoints resolve to different planes (arch/09 §1).
    #[error("plane mismatch: {0}")]
    PlaneMismatch(String),

    #[error("plane already exists: {0}")]
    PlaneExists(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("database is corrupt: {0}")]
    Corrupt(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
