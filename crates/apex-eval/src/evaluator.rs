use apex_core::ports::LlmProvider;

use crate::{adversarial, checks, parser, CriterionResult, EvalConfig, EvalOn, EvalResult, Evaluation};

pub struct Evaluator;

impl Evaluator {
    /// Run all deterministic acceptance criteria parsed from the message body.
    ///
    /// Returns `None` if no criteria are found (backwards compatible with pre-phase-5 tasks).
    pub async fn run_deterministic(body: &str) -> Option<EvalResult> {
        let criteria = parser::parse_criteria(body);
        if criteria.is_empty() {
            return None;
        }

        let mut results: Vec<CriterionResult> = Vec::with_capacity(criteria.len());

        for criterion in &criteria {
            let result = checks::run_check(criterion).await;
            results.push(result);
        }

        let total = results.len();
        let passed = results.iter().filter(|r| r.passed).count();
        let failed = total - passed;

        Some(EvalResult {
            total,
            passed,
            failed,
            results,
        })
    }

    /// Run the full evaluation pipeline: deterministic checks first, then
    /// adversarial evaluation if configured and applicable.
    pub async fn evaluate(
        task_body: &str,
        result_text: &str,
        evaluator_persona: &str,
        llm: &dyn LlmProvider,
        config: &EvalConfig,
    ) -> Evaluation {
        // 1. Run deterministic checks
        let deterministic = Self::run_deterministic(task_body).await;

        // 2. If deterministic failed, return early — no need for adversarial
        if let Some(ref det) = deterministic {
            if !det.all_passed() {
                return Evaluation {
                    deterministic,
                    adversarial: None,
                    passed: false,
                };
            }
        }

        // 3. Check if adversarial evaluation is needed
        let fuzzy_criteria = parser::parse_fuzzy_criteria(task_body);
        let should_run_adversarial = match config.eval_on {
            EvalOn::Always => true,
            EvalOn::Never => false,
            EvalOn::FuzzyCriteria => !fuzzy_criteria.is_empty(),
        };

        if !should_run_adversarial {
            return Evaluation {
                deterministic,
                adversarial: None,
                passed: true,
            };
        }

        // 4. Run adversarial evaluation
        let adversarial = match adversarial::run_adversarial(
            task_body,
            result_text,
            &fuzzy_criteria,
            evaluator_persona,
            llm,
        )
        .await
        {
            Ok(result) => Some(result),
            Err(err) => {
                // LLM error is non-blocking — log and treat as pass
                eprintln!("  adversarial eval error (non-blocking): {err}");
                None
            }
        };

        // 5. Combine results
        let passed = adversarial.as_ref().map_or(true, |a| a.passed);

        Evaluation {
            deterministic,
            adversarial,
            passed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn evaluator_returns_none_for_no_criteria() {
        let result = Evaluator::run_deterministic("# Task: test\n## Description\nStuff.\n").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn evaluator_runs_passing_criteria() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `true`
  expect: exit_code 0
- command: `echo hello`
  expect: output_contains "hello"
"#;
        let result = Evaluator::run_deterministic(body).await.unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(result.passed, 2);
        assert_eq!(result.failed, 0);
        assert!(result.all_passed());
    }

    #[tokio::test]
    async fn evaluator_runs_failing_criteria() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `false`
  expect: exit_code 0
"#;
        let result = Evaluator::run_deterministic(body).await.unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.passed, 0);
        assert_eq!(result.failed, 1);
        assert!(!result.all_passed());
    }

    #[tokio::test]
    async fn evaluator_mixed_results() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `true`
  expect: exit_code 0
- command: `false`
  expect: exit_code 0
"#;
        let result = Evaluator::run_deterministic(body).await.unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(result.passed, 1);
        assert_eq!(result.failed, 1);
        assert!(!result.all_passed());
    }

    #[tokio::test]
    async fn full_summary_format() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `true`
  expect: exit_code 0
- command: `false`
  expect: exit_code 0
"#;
        let result = Evaluator::run_deterministic(body).await.unwrap();
        let summary = result.full_summary();
        assert!(summary.contains("1/2 checks passed"));
        assert!(summary.contains("[pass]"));
        assert!(summary.contains("[FAIL]"));
    }

    #[tokio::test]
    async fn failure_summary_format() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `true`
  expect: exit_code 0
- command: `false`
  expect: exit_code 0
"#;
        let result = Evaluator::run_deterministic(body).await.unwrap();
        let summary = result.failure_summary();
        assert!(summary.contains("1/2 checks failed"));
        assert!(summary.contains("[FAIL]"));
        // Should NOT contain passing checks
        assert!(!summary.contains("[pass]"));
    }
}
