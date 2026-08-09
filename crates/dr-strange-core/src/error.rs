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

    /// A uniqueness constraint outside plane naming, e.g. an external key
    /// already bound to a different node in the same plane (arch/01 §2).
    #[error("conflict: {0}")]
    Conflict(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A bounded wait expired — currently only `begin_write` giving up on the
    /// writer slot (arch/01 §3, arch/04 §8.3). Distinct from [`Error::Conflict`]
    /// because nothing was attempted and nothing was lost: the caller simply
    /// never got the writer, so retrying is the correct response.
    #[error("timed out: {0}")]
    Timeout(String),

    #[error("database is corrupt: {0}")]
    Corrupt(String),

    /// A storage-backend failure. The concrete backend error (e.g. redb's)
    /// is preserved as the boxed source rather than appearing in this type:
    /// backends are swappable implementation details (arch/01 §1), so their
    /// error types must not become part of the public API.
    #[error("storage backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Maps a backend library error into [`Error::Backend`], keeping it as the
/// source for chained reporting / downcasting.
pub(crate) fn backend(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Backend(Box::new(e))
}

pub type Result<T> = std::result::Result<T, Error>;
