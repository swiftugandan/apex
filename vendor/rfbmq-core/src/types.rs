use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ID_LEN: usize = 32;
pub const MAX_PATH: usize = 4096;
pub const DEFAULT_RETRIES: u32 = 3;
pub const DEFAULT_LEASE: u32 = 300;
pub const DEFAULT_PURGE_AGE: u32 = 604_800;
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;
pub const SCAN_RETRIES: u32 = 3;
pub const DEFAULT_MAX_PENDING: i64 = 10_000;

// ── MessageId ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseMessageIdError(String);

impl fmt::Display for ParseMessageIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid message ID: '{}' (expected {} hex chars)", self.0, ID_LEN)
    }
}

impl std::error::Error for ParseMessageIdError {}

impl MessageId {
    pub fn generate() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 16];
        rng.fill(&mut bytes);
        let mut s = String::with_capacity(ID_LEN);
        for b in &bytes {
            use fmt::Write;
            write!(s, "{:02x}", b).unwrap();
        }
        MessageId(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for MessageId {
    type Err = ParseMessageIdError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.len() == ID_LEN && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(MessageId(s.to_string()))
        } else {
            Err(ParseMessageIdError(s.to_string()))
        }
    }
}

impl AsRef<str> for MessageId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ── ClaimedMessage ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ClaimedMessage {
    path: PathBuf,
    id: MessageId,
}

impl ClaimedMessage {
    pub fn from_path(path: impl Into<PathBuf>) -> std::result::Result<Self, crate::error::Error> {
        let path = path.into();
        let name = path
            .file_name()
            .ok_or_else(|| crate::error::Error::InvalidPath(path.display().to_string()))?
            .to_string_lossy();
        let stem = name.strip_suffix(".md")
            .ok_or_else(|| crate::error::Error::InvalidPath(path.display().to_string()))?;
        let dot_pos = stem.find('.')
            .ok_or_else(|| crate::error::Error::InvalidPath(path.display().to_string()))?;
        let id_str = &stem[dot_pos + 1..];
        let id: MessageId = id_str.parse()
            .map_err(|_| crate::error::Error::InvalidPath(path.display().to_string()))?;
        Ok(ClaimedMessage { path, id })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn id(&self) -> &MessageId {
        &self.id
    }
}

impl fmt::Display for ClaimedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Priority {
    Critical = 0,
    High = 1,
    Normal = 2,
    Low = 3,
}

impl Default for Priority {
    fn default() -> Self {
        Priority::Normal
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePriorityError(String);

impl std::fmt::Display for ParsePriorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown priority: '{}'", self.0)
    }
}

impl std::error::Error for ParsePriorityError {}

impl FromStr for Priority {
    type Err = ParsePriorityError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "critical" | "0" => Ok(Priority::Critical),
            "high" | "1" => Ok(Priority::High),
            "normal" | "2" => Ok(Priority::Normal),
            "low" | "3" => Ok(Priority::Low),
            _ => Err(ParsePriorityError(s.to_string())),
        }
    }
}

impl Priority {
    pub fn from_str_or_default(s: &str) -> Self {
        s.parse().unwrap_or_default()
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Critical => "critical",
            Priority::High => "high",
            Priority::Normal => "normal",
            Priority::Low => "low",
        }
    }

    pub fn dir_prefix(&self) -> &'static str {
        match self {
            Priority::Critical => "0-critical",
            Priority::High => "1-high",
            Priority::Normal => "2-normal",
            Priority::Low => "3-low",
        }
    }

    pub const ALL: [Priority; 4] = [
        Priority::Critical,
        Priority::High,
        Priority::Normal,
        Priority::Low,
    ];
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncMode {
    Full,
    /// Defers directory fsyncs — file data is synced but directory entries are
    /// not until an explicit `Queue::sync()` call. This is a valid optimization
    /// for batch workloads where crash recovery via `reap()` is acceptable.
    Batch,
    None,
}

impl Default for FsyncMode {
    fn default() -> Self {
        FsyncMode::Full
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub id: Option<MessageId>,
    pub created_at: String,
    pub created_by: String,
    pub priority: Priority,
    pub retry_count: u32,
    pub ttl: u32,
    pub tags: Vec<String>,
    pub correlation_id: Option<String>,
    pub reply_to: Option<String>,
    pub depends_on: Vec<MessageId>,
    pub custom: Vec<String>,
}

impl Default for Header {
    fn default() -> Self {
        Header {
            id: None,
            created_at: String::new(),
            created_by: String::new(),
            priority: Priority::Normal,
            retry_count: 0,
            ttl: 0,
            tags: Vec::new(),
            correlation_id: None,
            reply_to: None,
            depends_on: Vec::new(),
            custom: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub header: Header,
    pub body: String,
}

impl Default for Message {
    fn default() -> Self {
        Message {
            header: Header::default(),
            body: String::new(),
        }
    }
}

#[derive(Debug)]
pub struct Queue {
    root: PathBuf,
    max_retries: u32,
    lease_timeout: u32,
    use_priority_dirs: bool,
    fsync_mode: FsyncMode,
    file_mode: u32,
    dir_mode: u32,
    max_pending: i64,
}

impl Queue {
    pub(crate) fn new(
        root: PathBuf,
        use_priority_dirs: bool,
        max_pending: i64,
    ) -> Self {
        Queue {
            root,
            max_retries: DEFAULT_RETRIES,
            lease_timeout: DEFAULT_LEASE,
            use_priority_dirs,
            fsync_mode: FsyncMode::Full,
            file_mode: 0o640,
            dir_mode: 0o750,
            max_pending,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    pub fn lease_timeout(&self) -> u32 {
        self.lease_timeout
    }

    pub fn set_lease_timeout(&mut self, timeout: u32) {
        self.lease_timeout = timeout;
    }

    pub fn use_priority_dirs(&self) -> bool {
        self.use_priority_dirs
    }

    pub fn fsync_mode(&self) -> FsyncMode {
        self.fsync_mode
    }

    pub fn set_fsync_mode(&mut self, mode: FsyncMode) {
        self.fsync_mode = mode;
    }

    pub fn file_mode(&self) -> u32 {
        self.file_mode
    }

    pub fn dir_mode(&self) -> u32 {
        self.dir_mode
    }

    pub fn max_pending(&self) -> i64 {
        self.max_pending
    }
}
