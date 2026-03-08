pub mod config;
pub mod context;
pub mod domain;
pub mod error;
pub mod ports;

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
