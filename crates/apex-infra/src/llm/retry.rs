use std::time::Duration;

/// Exponential backoff with jitter, capped at 60s.
/// Prefers Retry-After header value when available.
pub fn backoff_delay(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    if let Some(secs) = retry_after_secs {
        return Duration::from_secs(secs.min(120));
    }
    let base_ms = ((1u64 << attempt) * 1000).min(60_000);
    let jitter = {
        use rand::Rng;
        rand::thread_rng().gen_range(0..=base_ms / 4)
    };
    Duration::from_millis(base_ms + jitter)
}
