use std::path::PathBuf;
use thiserror::Error;

/// Common error type for cache operations
#[derive(Error, Debug)]
pub enum CacheError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to initialize cache: {0}")]
    Initialization(String),

    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Content hash calculation failed for {path}: {reason}")]
    HashFailure { path: PathBuf, reason: String },
}

impl CacheError {
    /// Create a new initialization error
    pub fn initialization<S: ToString>(message: S) -> Self {
        Self::Initialization(message.to_string())
    }
}

/// A specialized result type for cache operations
pub type CacheResult<T> = std::result::Result<T, CacheError>;
