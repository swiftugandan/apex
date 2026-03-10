/// Maximum number of LLM turns in a single agentic loop.
pub const MAX_TURNS: usize = 32;

/// Maximum tokens per LLM completion request.
pub const MAX_TOKENS: u32 = 8192;

/// Number of empty poll cycles before a worker gives up.
pub const MAX_EMPTY_CYCLES: u32 = 300;

/// Base backoff in seconds for rate-limited retries.
pub const RATE_LIMIT_BACKOFF_SECS: u64 = 30;
