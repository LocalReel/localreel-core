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

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("job error: {0}")]
    Job(String),

    #[error("media operation failed: {0}")]
    Media(String),

    /// A media operation failure split into a message any signed-in account may
    /// see and a diagnostic detail (local paths, encoder stderr) that only an
    /// administrator gets.
    ///
    /// `Display` keeps both halves, so logs and the job history stay as
    /// informative as before; the HTTP layer decides what a response may show.
    #[error("media operation failed: {message}: {detail}")]
    MediaDetail { message: String, detail: String },

    #[error("internal lock poisoned: {0}")]
    LockPoisoned(String),
}

impl KernelError {
    /// Build a media failure whose detail must not reach an ordinary viewer.
    pub fn media_detail(message: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::MediaDetail {
            message: message.into(),
            detail: detail.into(),
        }
    }

    /// The text that is safe to show to any signed-in account.
    ///
    /// For [`KernelError::MediaDetail`] that is the summary only: a transcode
    /// failure must not leak the library's local paths or ffmpeg's stderr into
    /// a response body.
    pub fn public_message(&self) -> String {
        match self {
            Self::MediaDetail { message, .. } => {
                format!("media operation failed: {message}")
            }
            other => other.to_string(),
        }
    }

    /// Diagnostic text for administrators in debug mode.
    pub fn diagnostic_detail(&self) -> Option<&str> {
        match self {
            Self::MediaDetail { detail, .. } => Some(detail),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, KernelError>;
