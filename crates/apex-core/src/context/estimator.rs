use crate::domain::{CalibrationData, ContentType};

/// Stateful token estimator with content-type awareness and self-calibration.
#[derive(Clone, Default)]
pub struct TokenEstimator {
    calibration: CalibrationData,
}

impl TokenEstimator {
    pub fn new(calibration: CalibrationData) -> Self {
        Self { calibration }
    }

    /// Classify text content type using heuristics.
    pub fn classify(text: &str) -> ContentType {
        if text.is_empty() {
            return ContentType::Mixed;
        }

        let total_lines = text.lines().count().max(1);
        let code_indicators = text
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                trimmed.contains('{')
                    || trimmed.contains('}')
                    || trimmed.contains(';')
                    || trimmed.starts_with("fn ")
                    || trimmed.starts_with("def ")
                    || trimmed.starts_with("class ")
                    || trimmed.starts_with("//")
                    || trimmed.starts_with('#')
                    || (line.len() > 4 && line.starts_with("    "))
            })
            .count();

        let ratio = code_indicators as f64 / total_lines as f64;
        if ratio > 0.50 {
            ContentType::Code
        } else if ratio < 0.15 {
            ContentType::Prose
        } else {
            ContentType::Mixed
        }
    }

    /// Get the chars-per-token ratio for a content type.
    pub fn ratio(&self, ct: ContentType) -> f32 {
        match ct {
            ContentType::Prose => self.calibration.chars_per_token_prose,
            ContentType::Code => self.calibration.chars_per_token_code,
            ContentType::Mixed => self.calibration.chars_per_token_mixed,
        }
    }

    /// Estimate token count for text, auto-classifying content type.
    pub fn estimate(&self, text: &str) -> u32 {
        let ct = Self::classify(text);
        self.estimate_typed(text, ct)
    }

    /// Estimate token count for text with a specified content type.
    pub fn estimate_typed(&self, text: &str, ct: ContentType) -> u32 {
        let ratio = self.ratio(ct) as f64;
        if ratio <= 0.0 {
            return 0;
        }
        (text.len() as f64 / ratio).ceil() as u32
    }

    /// Truncate text to fit within a token budget, appending `[truncated]` if needed.
    /// Auto-classifies content type.
    pub fn budget(&self, text: &str, max_tokens: u32) -> String {
        let ct = Self::classify(text);
        let ratio = self.ratio(ct) as f64;
        let max_chars = (max_tokens as f64 * ratio) as usize;
        if text.len() <= max_chars {
            text.to_string()
        } else {
            let truncated = crate::truncate_str(text, max_chars.saturating_sub(12));
            format!("{truncated}\n[truncated]")
        }
    }

    /// Calibrate the estimator from actual LLM token usage using EMA.
    pub fn calibrate(&mut self, prompt_text: &str, actual_tokens: u32) {
        if actual_tokens == 0 || prompt_text.is_empty() {
            return;
        }

        let ct = Self::classify(prompt_text);
        let observed_ratio = prompt_text.len() as f32 / actual_tokens as f32;

        self.calibration.sample_count += 1;
        let alpha = (2.0_f32 / (self.calibration.sample_count as f32 + 1.0)).clamp(0.05, 0.5);

        match ct {
            ContentType::Prose => {
                self.calibration.chars_per_token_prose =
                    alpha * observed_ratio + (1.0 - alpha) * self.calibration.chars_per_token_prose;
            }
            ContentType::Code => {
                self.calibration.chars_per_token_code =
                    alpha * observed_ratio + (1.0 - alpha) * self.calibration.chars_per_token_code;
            }
            ContentType::Mixed => {
                self.calibration.chars_per_token_mixed =
                    alpha * observed_ratio + (1.0 - alpha) * self.calibration.chars_per_token_mixed;
            }
        }
    }

    /// Get a reference to the current calibration data.
    pub fn calibration_data(&self) -> &CalibrationData {
        &self.calibration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_empty() {
        let est = TokenEstimator::default();
        assert_eq!(est.estimate(""), 0);
    }

    #[test]
    fn estimate_short_text() {
        let est = TokenEstimator::default();
        // Prose text: 7 chars / 4.0 = 2 tokens (ceil)
        let tokens = est.estimate_typed("hello!!", ContentType::Prose);
        assert_eq!(tokens, 2);
    }

    #[test]
    fn budget_within_limit() {
        let est = TokenEstimator::default();
        let text = "short";
        assert_eq!(est.budget(text, 100), "short");
    }

    #[test]
    fn budget_truncates() {
        let est = TokenEstimator::default();
        let text = "a".repeat(1000);
        let result = est.budget(&text, 10);
        assert!(result.ends_with("[truncated]"));
        assert!(result.len() < 1000);
    }

    #[test]
    fn classify_prose() {
        let text = "This is a simple paragraph of text.\nIt has no code at all.\nJust words.";
        assert_eq!(TokenEstimator::classify(text), ContentType::Prose);
    }

    #[test]
    fn classify_code() {
        let text = "fn main() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n";
        assert_eq!(TokenEstimator::classify(text), ContentType::Code);
    }

    #[test]
    fn classify_mixed() {
        let text =
            "This function does X.\nIt works like this:\nfn foo() {\n    bar();\n}\nThat's it.";
        assert_eq!(TokenEstimator::classify(text), ContentType::Mixed);
    }

    #[test]
    fn calibrate_updates_ratio() {
        let mut est = TokenEstimator::default();
        let prose = "Hello world this is a test paragraph with some words in it.";
        let initial = est.calibration.chars_per_token_prose;

        // Simulate: 59 chars -> 15 tokens -> observed ratio 3.93
        est.calibrate(prose, 15);

        assert_ne!(est.calibration.chars_per_token_prose, initial);
        assert_eq!(est.calibration.sample_count, 1);
    }

    #[test]
    fn calibrate_ignores_empty() {
        let mut est = TokenEstimator::default();
        est.calibrate("", 10);
        assert_eq!(est.calibration.sample_count, 0);
        est.calibrate("hello", 0);
        assert_eq!(est.calibration.sample_count, 0);
    }

    #[test]
    fn different_ratios_per_content_type() {
        let est = TokenEstimator::default();
        assert!(est.ratio(ContentType::Prose) > est.ratio(ContentType::Code));
        assert!(est.ratio(ContentType::Mixed) > est.ratio(ContentType::Code));
        assert!(est.ratio(ContentType::Prose) > est.ratio(ContentType::Mixed));
    }

    #[test]
    fn budget_multibyte_no_panic() {
        let est = TokenEstimator::default();
        // Emoji are 4 bytes each; small budget forces truncation mid-codepoint
        let text = "😀😁😂🤣😃😄😅😆😇😈".repeat(10);
        let result = est.budget(&text, 2);
        assert!(result.ends_with("[truncated]"));
    }

    #[test]
    fn budget_cjk_no_panic() {
        let est = TokenEstimator::default();
        // CJK chars are 3 bytes each
        let text = "\u{4e00}".repeat(100);
        let result = est.budget(&text, 2);
        assert!(result.ends_with("[truncated]"));
    }
}
