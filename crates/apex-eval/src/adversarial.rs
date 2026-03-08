use apex_core::domain::{ChatMessage, CompletionRequest};
use apex_core::error::LlmError;
use apex_core::ports::LlmProvider;

use crate::{AdversarialFinding, AdversarialResult, FindingSeverity};

/// Run the adversarial evaluation pass.
///
/// Sends the task body, result text, and fuzzy criteria to an LLM with the
/// evaluator persona and parses the structured response.
pub async fn run_adversarial(
    task_body: &str,
    result_text: &str,
    fuzzy_criteria: &[String],
    evaluator_persona: &str,
    llm: &dyn LlmProvider,
) -> Result<AdversarialResult, LlmError> {
    let truncated_task = truncate_to_approx_tokens(task_body, 2000);
    let truncated_result = truncate_to_approx_tokens(result_text, 2000);

    let criteria_text = if fuzzy_criteria.is_empty() {
        "No specific fuzzy criteria provided. Evaluate overall quality.".to_string()
    } else {
        fuzzy_criteria
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let user_prompt = format!(
        "## Original Task\n\n{truncated_task}\n\n\
         ## Agent's Result\n\n{truncated_result}\n\n\
         ## Fuzzy Criteria\n\n{criteria_text}"
    );

    let req = CompletionRequest {
        system_prompt: evaluator_persona.to_string(),
        messages: vec![ChatMessage::user_text(&user_prompt)],
        max_tokens: 4096,
        temperature: Some(0.0),
    };

    let resp = llm.complete(req).await?;
    let raw_response = resp.message.text();
    let (blocking, warnings, passed) = parse_adversarial_response(&raw_response);

    Ok(AdversarialResult {
        passed,
        blocking_issues: blocking,
        warnings,
        raw_response,
        usage: resp.usage,
    })
}

/// Parse the adversarial evaluator's response into structured findings.
///
/// Extracts `[BLOCK]` lines, `[WARN]` lines, and the `PASS`/`FAIL` verdict.
/// If no verdict line is found, passes iff there are no blocking issues.
pub fn parse_adversarial_response(
    text: &str,
) -> (Vec<AdversarialFinding>, Vec<AdversarialFinding>, bool) {
    let mut blocking = Vec::new();
    let mut warnings = Vec::new();
    let mut verdict: Option<bool> = None;

    for line in text.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("- [BLOCK]") {
            blocking.push(AdversarialFinding {
                severity: FindingSeverity::Blocking,
                description: rest.trim().to_string(),
            });
        } else if let Some(rest) = trimmed.strip_prefix("- [WARN]") {
            warnings.push(AdversarialFinding {
                severity: FindingSeverity::Warning,
                description: rest.trim().to_string(),
            });
        } else if verdict.is_none() {
            let upper = trimmed.to_uppercase();
            if upper == "PASS" || upper == "**PASS**" {
                verdict = Some(true);
            } else if upper == "FAIL" || upper == "**FAIL**" {
                verdict = Some(false);
            }
        }
    }

    let passed = verdict.unwrap_or_else(|| blocking.is_empty());
    (blocking, warnings, passed)
}

/// Rough truncation: ~4 chars per token.
fn truncate_to_approx_tokens(text: &str, max_tokens: usize) -> &str {
    let max_chars = max_tokens * 4;
    if text.len() <= max_chars {
        text
    } else {
        // Find a safe UTF-8 boundary
        let mut end = max_chars;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pass_with_warnings() {
        let response = r#"## Blocking Issues
None.

## Warnings
- [WARN] The script doesn't compress rotated logs
- [WARN] No logging of rotation events

## Verdict
PASS
"#;
        let (blocking, warnings, passed) = parse_adversarial_response(response);
        assert!(passed);
        assert!(blocking.is_empty());
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            warnings[0].description,
            "The script doesn't compress rotated logs"
        );
        assert_eq!(warnings[1].severity, FindingSeverity::Warning);
    }

    #[test]
    fn parse_fail_with_blocking() {
        let response = r#"## Blocking Issues
- [BLOCK] Missing error handling for disk full scenario
- [BLOCK] Log rotation doesn't preserve file permissions

## Warnings
- [WARN] Could use more descriptive variable names

## Verdict
FAIL
"#;
        let (blocking, warnings, passed) = parse_adversarial_response(response);
        assert!(!passed);
        assert_eq!(blocking.len(), 2);
        assert_eq!(
            blocking[0].description,
            "Missing error handling for disk full scenario"
        );
        assert_eq!(blocking[0].severity, FindingSeverity::Blocking);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn parse_malformed_no_verdict() {
        let response = "Some random text without structure";
        let (blocking, warnings, passed) = parse_adversarial_response(response);
        assert!(passed); // No blocking issues → pass
        assert!(blocking.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn parse_malformed_blocks_no_verdict() {
        let response = "- [BLOCK] Something is wrong\nNo verdict line here";
        let (blocking, _warnings, passed) = parse_adversarial_response(response);
        assert!(!passed); // Has blocking issues → fail
        assert_eq!(blocking.len(), 1);
    }

    #[test]
    fn parse_bold_verdict() {
        let response = "## Verdict\n**PASS**\n";
        let (_blocking, _warnings, passed) = parse_adversarial_response(response);
        assert!(passed);
    }

    #[test]
    fn truncate_short_text() {
        let text = "short";
        assert_eq!(truncate_to_approx_tokens(text, 100), "short");
    }

    #[test]
    fn truncate_long_text() {
        let text = "a".repeat(10000);
        let result = truncate_to_approx_tokens(&text, 100);
        assert_eq!(result.len(), 400); // 100 tokens * 4 chars
    }
}
