use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("queue not found: {0}")]
    QueueNotFound(PathBuf),

    #[error("queue already exists: {0}")]
    QueueExists(PathBuf),

    #[error("max pending limit exceeded ({0})")]
    MaxPendingExceeded(i64),

    #[error("message too large: {size} bytes (max {max})")]
    MessageTooLarge { size: usize, max: usize },

    #[error("parse error: {0}")]
    Parse(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("file not found: {0}")]
    FileNotFound(PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;
