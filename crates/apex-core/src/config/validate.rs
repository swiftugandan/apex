use std::path::Path;

use super::agent::AgentConfig;
use super::invariants::Invariants;

/// A single validation issue.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
    pub severity: IssueSeverity,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IssueSeverity {
    Error,
    Warning,
}

/// Aggregate validation result.
#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        !self.issues.iter().any(|i| i.severity == IssueSeverity::Error)
    }

    pub fn errors(&self) -> Vec<&ValidationIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == IssueSeverity::Error)
            .collect()
    }

    pub fn display(&self) -> String {
        if self.issues.is_empty() {
            return "All checks passed.".to_string();
        }
        let mut out = String::new();
        for issue in &self.issues {
            let prefix = match issue.severity {
                IssueSeverity::Error => "ERROR",
                IssueSeverity::Warning => "WARN ",
            };
            out.push_str(&format!("[{}] {}: {}\n", prefix, issue.field, issue.message));
        }
        out
    }
}

/// Validate an agent config against invariant ceilings.
pub fn validate_against_invariants(
    config: &AgentConfig,
    invariants: &Invariants,
) -> ValidationReport {
    let mut issues = Vec::new();
    let limits = &invariants.limits;

    if config.agent.max_depth > limits.max_depth {
        issues.push(ValidationIssue {
            field: "agent.max_depth".to_string(),
            message: format!(
                "value {} exceeds invariant ceiling of {}",
                config.agent.max_depth, limits.max_depth
            ),
            severity: IssueSeverity::Error,
        });
    }

    if config.agent.max_concurrent > limits.max_concurrent {
        issues.push(ValidationIssue {
            field: "agent.max_concurrent".to_string(),
            message: format!(
                "value {} exceeds invariant ceiling of {}",
                config.agent.max_concurrent, limits.max_concurrent
            ),
            severity: IssueSeverity::Error,
        });
    }

    if config.agent.tools.len() > limits.max_tools {
        issues.push(ValidationIssue {
            field: "agent.tools".to_string(),
            message: format!(
                "tool count {} exceeds invariant ceiling of {}",
                config.agent.tools.len(),
                limits.max_tools
            ),
            severity: IssueSeverity::Error,
        });
    }

    if config.context_budget.max_body_tokens > limits.max_body_tokens {
        issues.push(ValidationIssue {
            field: "context_budget.max_body_tokens".to_string(),
            message: format!(
                "value {} exceeds invariant ceiling of {}",
                config.context_budget.max_body_tokens, limits.max_body_tokens
            ),
            severity: IssueSeverity::Error,
        });
    }

    if config.agent.max_retries > limits.max_retries {
        issues.push(ValidationIssue {
            field: "agent.max_retries".to_string(),
            message: format!(
                "value {} exceeds invariant ceiling of {}",
                config.agent.max_retries, limits.max_retries
            ),
            severity: IssueSeverity::Error,
        });
    }

    ValidationReport { issues }
}

/// Full validation: invariant ceilings + structural checks.
pub fn validate_full(
    config: &AgentConfig,
    invariants: &Invariants,
    prompts_dir: &Path,
) -> ValidationReport {
    let mut report = validate_against_invariants(config, invariants);

    // Check persona file exists
    let persona_path = prompts_dir.join("agent.md");
    if !persona_path.exists() {
        report.issues.push(ValidationIssue {
            field: "prompts/agent.md".to_string(),
            message: "persona file not found".to_string(),
            severity: IssueSeverity::Error,
        });
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn config_within_bounds_is_ok() {
        let config = AgentConfig::default();
        let inv = Invariants::default();
        let report = validate_against_invariants(&config, &inv);
        assert!(report.is_ok());
        assert!(report.issues.is_empty());
    }

    #[test]
    fn config_exceeding_ceiling_reports_error() {
        let mut config = AgentConfig::default();
        config.agent.max_depth = 10; // ceiling is 5

        let inv = Invariants::default();
        let report = validate_against_invariants(&config, &inv);
        assert!(!report.is_ok());
        assert_eq!(report.errors().len(), 1);
        assert_eq!(report.errors()[0].field, "agent.max_depth");
    }

    #[test]
    fn multiple_violations() {
        let mut config = AgentConfig::default();
        config.agent.max_depth = 10;
        config.agent.max_concurrent = 100;
        config.context_budget.max_body_tokens = 999_999;

        let inv = Invariants::default();
        let report = validate_against_invariants(&config, &inv);
        assert!(!report.is_ok());
        assert_eq!(report.errors().len(), 3);
    }

    #[test]
    fn validate_full_missing_persona() {
        let config = AgentConfig::default();
        let inv = Invariants::default();
        let dir = TempDir::new().unwrap();

        let report = validate_full(&config, &inv, dir.path());
        assert!(!report.is_ok());
        assert!(report.issues.iter().any(|i| i.field == "prompts/agent.md"));
    }

    #[test]
    fn validate_full_with_persona_ok() {
        let config = AgentConfig::default();
        let inv = Invariants::default();
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "# Agent").unwrap();

        let report = validate_full(&config, &inv, dir.path());
        assert!(report.is_ok());
    }
}
