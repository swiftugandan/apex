pub mod config;
pub mod context;
pub mod domain;
pub mod error;
pub mod ports;

pub use domain::LoopLimits;

/// Well-known tool name for the explicit completion protocol.
/// The agentic loop intercepts this tool call as a signal to end the loop
/// rather than dispatching it to a tool registry for execution.
pub const TASK_COMPLETE_TOOL: &str = "task_complete";

/// Truncate a string to at most `max_bytes` bytes at a valid UTF-8 char boundary.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Current time as a Unix timestamp string (e.g. "1709913600").
pub fn now_unix_ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

/// Generate a pseudo-unique ID with a prefix (e.g. "fact-00a1b2c3d4e5f67800ab").
pub fn generate_id(prefix: &str) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{prefix}-{:016x}{:04x}", t, pid & 0xFFFF)
}

/// Summarize a JSON value to a max length string.
pub fn summarize_json(value: &serde_json::Value, max_len: usize) -> String {
    let s = value.to_string();
    if s.len() <= max_len {
        s
    } else {
        let truncated = truncate_str(&s, max_len);
        format!("{truncated}…")
    }
}
