/// Rough token estimator using a fixed chars-per-token ratio.
/// Calibration from actual LLM responses deferred to Phase 8.
pub struct TokenEstimator;

const CHARS_PER_TOKEN: f64 = 3.5;

impl TokenEstimator {
    /// Estimate the token count for the given text.
    pub fn estimate(text: &str) -> u32 {
        (text.len() as f64 / CHARS_PER_TOKEN).ceil() as u32
    }

    /// Truncate text to fit within a token budget, appending `[truncated]` if needed.
    pub fn budget(text: &str, max_tokens: u32) -> String {
        let max_chars = (max_tokens as f64 * CHARS_PER_TOKEN) as usize;
        if text.len() <= max_chars {
            text.to_string()
        } else {
            let truncated = &text[..max_chars.saturating_sub(12)];
            format!("{truncated}\n[truncated]")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_empty() {
        assert_eq!(TokenEstimator::estimate(""), 0);
    }

    #[test]
    fn estimate_short_text() {
        // 7 chars / 3.5 = 2 tokens
        assert_eq!(TokenEstimator::estimate("hello!!"), 2);
    }

    #[test]
    fn budget_within_limit() {
        let text = "short";
        assert_eq!(TokenEstimator::budget(text, 100), "short");
    }

    #[test]
    fn budget_truncates() {
        let text = "a".repeat(1000);
        let result = TokenEstimator::budget(&text, 10);
        assert!(result.ends_with("[truncated]"));
        assert!(result.len() < 1000);
    }
}
