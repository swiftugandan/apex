pub mod error;
pub mod fs_utils;
pub mod message;
pub mod queue;
pub mod types;

pub use error::{Error, Result};
#[allow(deprecated)]
pub use message::{generate_id, parse_file, parse_headers};
pub use types::{
    ClaimedMessage, FsyncMode, Header, Message, MessageId, ParseMessageIdError,
    ParsePriorityError, Priority, Queue,
};
pub use types::{
    DEFAULT_LEASE, DEFAULT_MAX_PENDING, DEFAULT_PURGE_AGE, DEFAULT_RETRIES, ID_LEN,
    MAX_MESSAGE_SIZE, MAX_PATH, SCAN_RETRIES, VERSION,
};
