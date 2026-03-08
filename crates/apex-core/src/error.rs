use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http error: {0}")]
    Http(String),
    #[error("api error: {0}")]
    Api(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
    #[error("configuration error: {0}")]
    Configuration(String),
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("execution failed: {0}")]
    Execution(String),
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory I/O error: {0}")]
    Io(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("database error: {0}")]
    Database(String),
}

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("queue not found: {0}")]
    NotFound(String),
    #[error("queue already exists: {0}")]
    AlreadyExists(String),
    #[error("queue is empty")]
    Empty,
    #[error("queue is full")]
    Full,
    #[error("queue I/O error: {0}")]
    Io(String),
    #[error("message parse error: {0}")]
    Parse(String),
}
