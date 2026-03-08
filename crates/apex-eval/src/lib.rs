pub mod adversarial;
pub mod checks;
pub mod evaluator;
pub mod parser;

/// A single acceptance criterion parsed from the task body.
#[derive(Debug, Clone)]
pub struct Criterion {
    /// Shell command to execute.
    pub command: String,
    /// What to assert on the result.
    pub check: CheckType,
}

/// The type of assertion to run against a command's output.
#[derive(Debug, Clone)]
pub enum CheckType {
    ExitCode(i32),
    OutputContains(String),
    OutputMatches(String), // regex
    FileExists(String),
    FileContains { path: String, expected: String },
    FileSizeRange { path: String, min: u64, max: u64 },
    HttpStatus { url: String, expected: u16 },
    JsonPath { path: String, expected: String },
    NotContains(String),
}

/// Result of running a single criterion check.
#[derive(Debug, Clone)]
pub struct CriterionResult {
    /// Human-readable description of what was checked.
    pub criterion_display: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Actual value observed.
    pub actual: String,
}

/// Aggregate result of running all criteria for a task.
#[derive(Debug, Clone)]
pub struct EvalResult {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub results: Vec<CriterionResult>,
}

impl EvalResult {
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }

    /// Full markdown summary with checkmarks for all criteria.
    pub fn full_summary(&self) -> String {
        let mut out = format!(
            "**{}/{} checks passed**\n\n",
            self.passed, self.total
        );
        for r in &self.results {
            let mark = if r.passed { "pass" } else { "FAIL" };
            out.push_str(&format!("- [{}] {}\n", mark, r.criterion_display));
            if !r.passed {
                out.push_str(&format!("  actual: {}\n", r.actual));
            }
        }
        out
    }

    /// Markdown listing only failed criteria (for retry context).
    pub fn failure_summary(&self) -> String {
        let mut out = format!(
            "**{}/{} checks failed**\n\n",
            self.failed, self.total
        );
        for r in &self.results {
            if !r.passed {
                out.push_str(&format!("- [FAIL] {}\n", r.criterion_display));
                out.push_str(&format!("  actual: {}\n", r.actual));
            }
        }
        out
    }
}

// -- Adversarial evaluation types --

/// When to run adversarial evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalOn {
    /// Always run adversarial evaluation.
    Always,
    /// Only run when fuzzy criteria are present.
    FuzzyCriteria,
    /// Never run adversarial evaluation.
    Never,
}

/// Configuration for the evaluation pipeline.
#[derive(Debug, Clone)]
pub struct EvalConfig {
    pub eval_model: Option<String>,
    pub eval_on: EvalOn,
}

impl EvalConfig {
    pub fn from_env() -> Self {
        let eval_model = std::env::var("APEX_EVAL_MODEL").ok();

        let eval_on = match std::env::var("APEX_EVAL_ON")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "always" => EvalOn::Always,
            "never" => EvalOn::Never,
            _ => EvalOn::FuzzyCriteria,
        };

        Self { eval_model, eval_on }
    }
}

/// Severity of an adversarial finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingSeverity {
    Blocking,
    Warning,
}

/// A single finding from the adversarial evaluator.
#[derive(Debug, Clone)]
pub struct AdversarialFinding {
    pub severity: FindingSeverity,
    pub description: String,
}

/// Result of the adversarial evaluation pass.
#[derive(Debug, Clone)]
pub struct AdversarialResult {
    pub passed: bool,
    pub blocking_issues: Vec<AdversarialFinding>,
    pub warnings: Vec<AdversarialFinding>,
    pub raw_response: String,
    pub usage: apex_core::domain::TokenUsage,
}

/// Combined result of both evaluation layers.
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub deterministic: Option<EvalResult>,
    pub adversarial: Option<AdversarialResult>,
    pub passed: bool,
}

impl Evaluation {
    /// Full markdown summary of both evaluation layers.
    pub fn full_summary(&self) -> String {
        let mut out = String::new();

        if let Some(det) = &self.deterministic {
            out.push_str(&format!(
                "### Deterministic: {} ({}/{})\n",
                if det.all_passed() { "PASS" } else { "FAIL" },
                det.passed,
                det.total,
            ));
            for r in &det.results {
                let mark = if r.passed { "pass" } else { "FAIL" };
                out.push_str(&format!("- [{}] {}\n", mark, r.criterion_display));
                if !r.passed {
                    out.push_str(&format!("  actual: {}\n", r.actual));
                }
            }
        }

        if let Some(adv) = &self.adversarial {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!(
                "### Adversarial: {}\n",
                if adv.passed { "PASS" } else { "FAIL" },
            ));

            if adv.blocking_issues.is_empty() && adv.warnings.is_empty() {
                out.push_str("No issues found.\n");
            } else {
                for finding in &adv.blocking_issues {
                    out.push_str(&format!("- [BLOCK] {}\n", finding.description));
                }
                for finding in &adv.warnings {
                    out.push_str(&format!("- [WARN] {}\n", finding.description));
                }
            }
        }

        out
    }

    /// Markdown summary of only failures (for retry context).
    pub fn failure_summary(&self) -> String {
        let mut out = String::new();

        if let Some(det) = &self.deterministic {
            if !det.all_passed() {
                out.push_str(&format!(
                    "### Deterministic: FAIL ({}/{})\n",
                    det.failed, det.total,
                ));
                for r in &det.results {
                    if !r.passed {
                        out.push_str(&format!("- [FAIL] {}\n", r.criterion_display));
                        out.push_str(&format!("  actual: {}\n", r.actual));
                    }
                }
            }
        }

        if let Some(adv) = &self.adversarial {
            if !adv.passed {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("### Adversarial: FAIL\n");
                if adv.blocking_issues.is_empty() {
                    // No structured findings parsed — include truncated raw response
                    let raw = adv.raw_response.trim();
                    let truncated = apex_core::truncate_str(raw, 500);
                    out.push_str(truncated);
                    if truncated.len() < raw.len() {
                        out.push_str("...");
                    }
                    out.push('\n');
                } else {
                    for finding in &adv.blocking_issues {
                        out.push_str(&format!("- [BLOCK] {}\n", finding.description));
                    }
                }
            }
        }

        out
    }
}

pub use evaluator::Evaluator;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_config_from_env_defaults() {
        // Clear env vars to test defaults
        std::env::remove_var("APEX_EVAL_MODEL");
        std::env::remove_var("APEX_EVAL_ON");

        let config = EvalConfig::from_env();
        assert!(config.eval_model.is_none());
        assert_eq!(config.eval_on, EvalOn::FuzzyCriteria);
    }

    #[test]
    fn eval_config_from_env_always() {
        std::env::set_var("APEX_EVAL_ON", "always");
        let config = EvalConfig::from_env();
        assert_eq!(config.eval_on, EvalOn::Always);
        std::env::remove_var("APEX_EVAL_ON");
    }

    #[test]
    fn eval_config_from_env_never() {
        std::env::set_var("APEX_EVAL_ON", "never");
        let config = EvalConfig::from_env();
        assert_eq!(config.eval_on, EvalOn::Never);
        std::env::remove_var("APEX_EVAL_ON");
    }

    #[test]
    fn eval_config_from_env_model() {
        std::env::set_var("APEX_EVAL_MODEL", "claude-3-haiku-20240307");
        std::env::remove_var("APEX_EVAL_ON");
        let config = EvalConfig::from_env();
        assert_eq!(
            config.eval_model.as_deref(),
            Some("claude-3-haiku-20240307")
        );
        std::env::remove_var("APEX_EVAL_MODEL");
    }

    #[test]
    fn evaluation_full_summary_both_layers() {
        let eval = Evaluation {
            deterministic: Some(EvalResult {
                total: 2,
                passed: 2,
                failed: 0,
                results: vec![
                    CriterionResult {
                        criterion_display: "`true` → exit 0".into(),
                        passed: true,
                        actual: "exit code 0".into(),
                    },
                ],
            }),
            adversarial: Some(AdversarialResult {
                passed: true,
                blocking_issues: vec![],
                warnings: vec![AdversarialFinding {
                    severity: FindingSeverity::Warning,
                    description: "minor style concern".into(),
                }],
                raw_response: String::new(),
                usage: apex_core::domain::TokenUsage::default(),
            }),
            passed: true,
        };

        let summary = eval.full_summary();
        assert!(summary.contains("### Deterministic: PASS"));
        assert!(summary.contains("### Adversarial: PASS"));
        assert!(summary.contains("[WARN] minor style concern"));
    }

    #[test]
    fn evaluation_failure_summary_adversarial_fail() {
        let eval = Evaluation {
            deterministic: Some(EvalResult {
                total: 1,
                passed: 1,
                failed: 0,
                results: vec![],
            }),
            adversarial: Some(AdversarialResult {
                passed: false,
                blocking_issues: vec![AdversarialFinding {
                    severity: FindingSeverity::Blocking,
                    description: "missing error handling".into(),
                }],
                warnings: vec![],
                raw_response: String::new(),
                usage: apex_core::domain::TokenUsage::default(),
            }),
            passed: false,
        };

        let summary = eval.failure_summary();
        assert!(summary.contains("### Adversarial: FAIL"));
        assert!(summary.contains("[BLOCK] missing error handling"));
        // Deterministic passed, so no deterministic failure section
        assert!(!summary.contains("### Deterministic: FAIL"));
    }

    #[test]
    fn evaluation_failure_summary_deterministic_fail() {
        let eval = Evaluation {
            deterministic: Some(EvalResult {
                total: 2,
                passed: 1,
                failed: 1,
                results: vec![
                    CriterionResult {
                        criterion_display: "`true` → exit 0".into(),
                        passed: true,
                        actual: "exit code 0".into(),
                    },
                    CriterionResult {
                        criterion_display: "`false` → exit 0".into(),
                        passed: false,
                        actual: "exit code 1".into(),
                    },
                ],
            }),
            adversarial: None,
            passed: false,
        };

        let summary = eval.failure_summary();
        assert!(summary.contains("### Deterministic: FAIL (1/2)"));
        assert!(summary.contains("[FAIL] `false` → exit 0"));
        assert!(!summary.contains("[pass]")); // Only failures
    }
}
