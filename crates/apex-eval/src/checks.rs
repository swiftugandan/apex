use std::time::Duration;
use tokio::process::Command;

use crate::{CheckType, Criterion, CriterionResult};

/// Run a single criterion check by executing its shell command and comparing the result.
pub async fn run_check(criterion: &Criterion) -> CriterionResult {
    let display = format_criterion_display(criterion);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("/bin/sh")
            .arg("-c")
            .arg(&criterion.command)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let _stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            match &criterion.check {
                CheckType::ExitCode(expected) => CriterionResult {
                    criterion_display: display,
                    passed: exit_code == *expected,
                    actual: format!("exit_code {exit_code}"),
                },
                CheckType::OutputContains(expected) => CriterionResult {
                    criterion_display: display,
                    passed: stdout.contains(expected.as_str()),
                    actual: truncate(&stdout, 200),
                },
                CheckType::OutputMatches(pattern) => {
                    let passed = regex::Regex::new(pattern)
                        .map(|re| re.is_match(&stdout))
                        .unwrap_or(false);
                    CriterionResult {
                        criterion_display: display,
                        passed,
                        actual: truncate(&stdout, 200),
                    }
                }
                CheckType::NotContains(unexpected) => CriterionResult {
                    criterion_display: display,
                    passed: !stdout.contains(unexpected.as_str()),
                    actual: truncate(&stdout, 200),
                },
                CheckType::FileExists(path) => CriterionResult {
                    criterion_display: display,
                    passed: exit_code == 0,
                    actual: if exit_code == 0 {
                        format!("file exists: {path}")
                    } else {
                        format!("file not found: {path}")
                    },
                },
                CheckType::FileContains { path, expected } => CriterionResult {
                    criterion_display: display,
                    passed: exit_code == 0,
                    actual: if exit_code == 0 {
                        format!("{path} contains \"{expected}\"")
                    } else {
                        format!("{path} does not contain \"{expected}\"")
                    },
                },
                CheckType::FileSizeRange { path, min, max } => {
                    let size: u64 = stdout.trim().parse().unwrap_or(0);
                    CriterionResult {
                        criterion_display: display,
                        passed: size >= *min && size <= *max,
                        actual: format!("{path} size={size} (expected {min}..{max})"),
                    }
                }
                CheckType::HttpStatus { url: _, expected } => {
                    let code_str = stdout.trim();
                    let actual_code: u16 = code_str.parse().unwrap_or(0);
                    CriterionResult {
                        criterion_display: display,
                        passed: actual_code == *expected,
                        actual: format!("http_status {actual_code}"),
                    }
                }
                CheckType::JsonPath { path, expected } => {
                    // Simple JSON path: try to parse output as JSON and extract
                    let passed = serde_json::from_str::<serde_json::Value>(&stdout)
                        .ok()
                        .and_then(|v| extract_json_path(&v, path))
                        .map(|v| v == *expected)
                        .unwrap_or(false);
                    CriterionResult {
                        criterion_display: display,
                        passed,
                        actual: truncate(&stdout, 200),
                    }
                }
            }
        }
        Ok(Err(e)) => CriterionResult {
            criterion_display: display,
            passed: false,
            actual: format!("execution error: {e}"),
        },
        Err(_) => CriterionResult {
            criterion_display: display,
            passed: false,
            actual: "timeout (30s)".to_string(),
        },
    }
}

/// Format a human-readable display string for a criterion.
fn format_criterion_display(criterion: &Criterion) -> String {
    match &criterion.check {
        CheckType::ExitCode(code) => format!("`{}` exits {code}", criterion.command),
        CheckType::OutputContains(s) => format!("`{}` output contains \"{s}\"", criterion.command),
        CheckType::OutputMatches(p) => format!("`{}` output matches /{p}/", criterion.command),
        CheckType::NotContains(s) => format!("`{}` output does not contain \"{s}\"", criterion.command),
        CheckType::FileExists(path) => format!("file exists: {path}"),
        CheckType::FileContains { path, expected } => format!("{path} contains \"{expected}\""),
        CheckType::FileSizeRange { path, min, max } => format!("{path} size in {min}..{max}"),
        CheckType::HttpStatus { url, expected } => format!("HTTP {expected} from {url}"),
        CheckType::JsonPath { path, expected } => format!("json {path} == \"{expected}\""),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

/// Simple JSON path extraction (supports `$.key.nested` style).
fn extract_json_path(value: &serde_json::Value, path: &str) -> Option<String> {
    let path = path.strip_prefix("$.").unwrap_or(path);
    let mut current = value;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    match current {
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_exit_code_success() {
        let criterion = Criterion {
            command: "true".to_string(),
            check: CheckType::ExitCode(0),
        };
        let result = run_check(&criterion).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn check_exit_code_failure() {
        let criterion = Criterion {
            command: "false".to_string(),
            check: CheckType::ExitCode(0),
        };
        let result = run_check(&criterion).await;
        assert!(!result.passed);
    }

    #[tokio::test]
    async fn check_output_contains() {
        let criterion = Criterion {
            command: "echo hello world".to_string(),
            check: CheckType::OutputContains("hello".to_string()),
        };
        let result = run_check(&criterion).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn check_output_contains_missing() {
        let criterion = Criterion {
            command: "echo hello world".to_string(),
            check: CheckType::OutputContains("goodbye".to_string()),
        };
        let result = run_check(&criterion).await;
        assert!(!result.passed);
    }

    #[tokio::test]
    async fn check_output_matches_regex() {
        let criterion = Criterion {
            command: "echo hello123".to_string(),
            check: CheckType::OutputMatches(r"hello\d+".to_string()),
        };
        let result = run_check(&criterion).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn check_not_contains() {
        let criterion = Criterion {
            command: "echo hello".to_string(),
            check: CheckType::NotContains("error".to_string()),
        };
        let result = run_check(&criterion).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn check_not_contains_fails_when_present() {
        let criterion = Criterion {
            command: "echo error occurred".to_string(),
            check: CheckType::NotContains("error".to_string()),
        };
        let result = run_check(&criterion).await;
        assert!(!result.passed);
    }

    #[tokio::test]
    async fn check_file_exists() {
        let criterion = Criterion {
            command: "test -f /bin/sh".to_string(),
            check: CheckType::FileExists("/bin/sh".to_string()),
        };
        let result = run_check(&criterion).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn check_file_not_exists() {
        let criterion = Criterion {
            command: "test -f /nonexistent/file/path".to_string(),
            check: CheckType::FileExists("/nonexistent/file/path".to_string()),
        };
        let result = run_check(&criterion).await;
        assert!(!result.passed);
    }

    #[tokio::test]
    async fn check_json_path() {
        let criterion = Criterion {
            command: r#"echo '{"name":"apex","version":"1.0"}'"#.to_string(),
            check: CheckType::JsonPath {
                path: "$.name".to_string(),
                expected: "apex".to_string(),
            },
        };
        let result = run_check(&criterion).await;
        assert!(result.passed);
    }

    #[test]
    fn extract_json_path_nested() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"a":{"b":"c"}}"#).unwrap();
        assert_eq!(extract_json_path(&value, "$.a.b"), Some("c".to_string()));
    }

    #[test]
    fn extract_json_path_missing() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"a":"b"}"#).unwrap();
        assert_eq!(extract_json_path(&value, "$.x"), None);
    }
}
