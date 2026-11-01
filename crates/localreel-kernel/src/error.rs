use thiserror::Error;

/// Errors produced by the kernel. The API crate maps these to HTTP responses;
/// the media crate adds domain-specific context where needed.
#[derive(Debug, Error)]
pub enum KernelError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("job error: {0}")]
    Job(String),

    #[error("media operation failed: {0}")]
    Media(String),

    #[error("internal lock poisoned: {0}")]
    LockPoisoned(String),
}

pub type Result<T> = std::result::Result<T, KernelError>;
