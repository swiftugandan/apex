use crate::{CheckType, Criterion};

/// Parse acceptance criteria from the `## Acceptance Criteria` / `### Deterministic`
/// section of a markdown message body.
///
/// Returns an empty vec if no criteria section is found (backwards compatible).
pub fn parse_criteria(body: &str) -> Vec<Criterion> {
    let section = extract_deterministic_section(body);
    if section.is_empty() {
        return Vec::new();
    }

    let mut criteria = Vec::new();
    let mut current_command: Option<String> = None;

    for line in section.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("- command: ") {
            // Save any pending command (shouldn't happen with well-formed input)
            current_command = Some(strip_backticks(rest));
        } else if let Some(rest) = trimmed.strip_prefix("expect: ") {
            if let Some(cmd) = current_command.take() {
                if let Some(criterion) = parse_expect(&cmd, rest.trim()) {
                    criteria.push(criterion);
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("- file_exists ") {
            let path = strip_quotes(rest);
            criteria.push(Criterion {
                command: format!("test -f {}", shell_quote(&path)),
                check: CheckType::ExitCode(0),
            });
        } else if let Some(rest) = trimmed.strip_prefix("- file_contains ") {
            if let Some((path, expected)) = parse_two_quoted_args(rest) {
                criteria.push(Criterion {
                    command: format!("grep -qF {} {}", shell_quote(&expected), shell_quote(&path)),
                    check: CheckType::ExitCode(0),
                });
            }
        } else if let Some(rest) = trimmed.strip_prefix("- http_status ") {
            if let Some((url, code_str)) = parse_two_args(rest) {
                if let Ok(code) = code_str.parse::<u16>() {
                    criteria.push(Criterion {
                        command: format!(
                            "curl -s -o /dev/null -w \"%{{http_code}}\" {}",
                            shell_quote(&url)
                        ),
                        check: CheckType::OutputContains(code.to_string()),
                    });
                }
            }
        }
    }

    criteria
}

/// Parse fuzzy criteria from the `### Fuzzy` section under `## Acceptance Criteria`.
///
/// Returns an empty vec if no fuzzy section is found.
pub fn parse_fuzzy_criteria(body: &str) -> Vec<String> {
    let section = extract_fuzzy_section(body);
    if section.is_empty() {
        return Vec::new();
    }

    section
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed.strip_prefix("- ").map(|rest| rest.to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Extract the content under `### Fuzzy` within `## Acceptance Criteria`.
fn extract_fuzzy_section(body: &str) -> String {
    let mut in_acceptance = false;
    let mut in_fuzzy = false;
    let mut lines = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();

        if trimmed == "## Acceptance Criteria" {
            in_acceptance = true;
            continue;
        }

        if in_acceptance && trimmed == "### Fuzzy" {
            in_fuzzy = true;
            continue;
        }

        if in_fuzzy {
            if trimmed.starts_with("## ") || (trimmed.starts_with("### ") && trimmed != "### Fuzzy")
            {
                break;
            }
            lines.push(line);
        }

        if in_acceptance
            && !in_fuzzy
            && trimmed.starts_with("## ")
            && trimmed != "## Acceptance Criteria"
        {
            break;
        }
    }

    lines.join("\n")
}

/// Extract the content under `### Deterministic` within `## Acceptance Criteria`.
fn extract_deterministic_section(body: &str) -> String {
    let mut in_acceptance = false;
    let mut in_deterministic = false;
    let mut lines = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();

        if trimmed == "## Acceptance Criteria" {
            in_acceptance = true;
            continue;
        }

        if in_acceptance && trimmed == "### Deterministic" {
            in_deterministic = true;
            continue;
        }

        // Stop at next ## or ### heading
        if in_deterministic {
            if trimmed.starts_with("## ") || (trimmed.starts_with("### ") && trimmed != "### Deterministic") {
                break;
            }
            lines.push(line);
        }

        // Stop acceptance section at next ## heading
        if in_acceptance && !in_deterministic && trimmed.starts_with("## ") && trimmed != "## Acceptance Criteria" {
            break;
        }
    }

    // If no ### Deterministic found, try parsing criteria directly under ## Acceptance Criteria
    if !in_deterministic && in_acceptance {
        let mut fallback_lines = Vec::new();
        let mut in_section = false;
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed == "## Acceptance Criteria" {
                in_section = true;
                continue;
            }
            if in_section {
                if trimmed.starts_with("## ") {
                    break;
                }
                fallback_lines.push(line);
            }
        }
        return fallback_lines.join("\n");
    }

    lines.join("\n")
}

/// Parse an `expect:` value into a CheckType.
fn parse_expect(command: &str, expect: &str) -> Option<Criterion> {
    if let Some(rest) = expect.strip_prefix("exit_code ") {
        let code: i32 = rest.trim().parse().ok()?;
        Some(Criterion {
            command: command.to_string(),
            check: CheckType::ExitCode(code),
        })
    } else if let Some(rest) = expect.strip_prefix("output_contains ") {
        Some(Criterion {
            command: command.to_string(),
            check: CheckType::OutputContains(strip_quotes(rest)),
        })
    } else if let Some(rest) = expect.strip_prefix("output_matches ") {
        Some(Criterion {
            command: command.to_string(),
            check: CheckType::OutputMatches(strip_quotes(rest)),
        })
    } else if let Some(rest) = expect.strip_prefix("not_contains ") {
        Some(Criterion {
            command: command.to_string(),
            check: CheckType::NotContains(strip_quotes(rest)),
        })
    } else if let Some(rest) = expect.strip_prefix("file_exists ") {
        let path = strip_quotes(rest);
        Some(Criterion {
            command: format!("test -f {}", shell_quote(&path)),
            check: CheckType::FileExists(path),
        })
    } else if let Some(rest) = expect.strip_prefix("file_contains ") {
        let (path, expected) = parse_two_quoted_args(rest)?;
        Some(Criterion {
            command: format!("grep -qF {} {}", shell_quote(&expected), shell_quote(&path)),
            check: CheckType::FileContains { path, expected },
        })
    } else if let Some(rest) = expect.strip_prefix("file_size ") {
        // file_size "/path" 100 2000
        let parts: Vec<&str> = rest.splitn(3, ' ').collect();
        if parts.len() == 3 {
            let path = strip_quotes(parts[0]);
            let min: u64 = parts[1].parse().ok()?;
            let max: u64 = parts[2].parse().ok()?;
            Some(Criterion {
                command: format!("stat -f%z {} 2>/dev/null || stat -c%s {}", shell_quote(&path), shell_quote(&path)),
                check: CheckType::FileSizeRange { path, min, max },
            })
        } else {
            None
        }
    } else if let Some(rest) = expect.strip_prefix("http_status ") {
        let (url, code_str) = parse_two_args(rest)?;
        let expected: u16 = code_str.parse().ok()?;
        Some(Criterion {
            command: format!(
                "curl -s -o /dev/null -w \"%{{http_code}}\" {}",
                shell_quote(&url)
            ),
            check: CheckType::HttpStatus { url, expected },
        })
    } else if let Some(rest) = expect.strip_prefix("json_path ") {
        let (path, expected) = parse_two_quoted_args(rest)?;
        Some(Criterion {
            command: command.to_string(),
            check: CheckType::JsonPath { path, expected },
        })
    } else {
        None
    }
}

fn strip_backticks(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('`') && s.ends_with('`') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Parse two quoted arguments: `"arg1" "arg2"`.
fn parse_two_quoted_args(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        let first = &rest[..end];
        let remainder = rest[end + 1..].trim();
        let second = strip_quotes(remainder);
        Some((first.to_string(), second))
    } else {
        let parts: Vec<&str> = s.splitn(2, ' ').collect();
        if parts.len() == 2 {
            Some((strip_quotes(parts[0]), strip_quotes(parts[1])))
        } else {
            None
        }
    }
}

/// Parse two space-separated arguments (first may be quoted).
fn parse_two_args(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        let first = &rest[..end];
        let remainder = rest[end + 1..].trim();
        Some((first.to_string(), remainder.to_string()))
    } else {
        let parts: Vec<&str> = s.splitn(2, ' ').collect();
        if parts.len() == 2 {
            Some((parts[0].to_string(), parts[1].to_string()))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exit_code_criterion() {
        let body = r#"# Task: test
## Acceptance Criteria
### Deterministic
- command: `which echo`
  expect: exit_code 0
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert_eq!(criteria[0].command, "which echo");
        assert!(matches!(criteria[0].check, CheckType::ExitCode(0)));
    }

    #[test]
    fn parse_output_contains_criterion() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `echo hello world`
  expect: output_contains "hello"
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(&criteria[0].check, CheckType::OutputContains(s) if s == "hello"));
    }

    #[test]
    fn parse_output_matches_criterion() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `echo hello123`
  expect: output_matches "hello\d+"
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(&criteria[0].check, CheckType::OutputMatches(s) if s == r"hello\d+"));
    }

    #[test]
    fn parse_not_contains_criterion() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `echo hello`
  expect: not_contains "error"
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(&criteria[0].check, CheckType::NotContains(s) if s == "error"));
    }

    #[test]
    fn parse_file_exists_shorthand() {
        let body = r#"## Acceptance Criteria
### Deterministic
- file_exists "/tmp/test.txt"
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert_eq!(criteria[0].command, "test -f '/tmp/test.txt'");
        assert!(matches!(criteria[0].check, CheckType::ExitCode(0)));
    }

    #[test]
    fn parse_file_contains_shorthand() {
        let body = r#"## Acceptance Criteria
### Deterministic
- file_contains "/tmp/test.txt" "hello"
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert!(criteria[0].command.contains("grep"));
    }

    #[test]
    fn parse_http_status_shorthand() {
        let body = r#"## Acceptance Criteria
### Deterministic
- http_status "http://localhost:8080" 200
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert!(criteria[0].command.contains("curl"));
    }

    #[test]
    fn parse_multiple_criteria() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `which nginx`
  expect: exit_code 0
- command: `curl -s localhost`
  expect: output_contains "Welcome to nginx"
- file_exists "/etc/nginx/nginx.conf"
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 3);
    }

    #[test]
    fn parse_empty_body() {
        assert!(parse_criteria("").is_empty());
    }

    #[test]
    fn parse_no_criteria_section() {
        let body = "# Task: something\n## Description\nDo stuff.\n";
        assert!(parse_criteria(body).is_empty());
    }

    #[test]
    fn parse_acceptance_criteria_without_deterministic_subsection() {
        let body = r#"## Acceptance Criteria
- command: `true`
  expect: exit_code 0
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
    }

    #[test]
    fn parse_criteria_stops_at_next_section() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `true`
  expect: exit_code 0
## Previous Attempts
Some other stuff
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
    }

    #[test]
    fn parse_file_size_criterion() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `ls`
  expect: file_size "/tmp/test.txt" 100 2000
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(&criteria[0].check, CheckType::FileSizeRange { min: 100, max: 2000, .. }));
    }

    #[test]
    fn parse_fuzzy_empty_body() {
        assert!(parse_fuzzy_criteria("").is_empty());
    }

    #[test]
    fn parse_fuzzy_no_section() {
        let body = "# Task: something\n## Description\nDo stuff.\n";
        assert!(parse_fuzzy_criteria(body).is_empty());
    }

    #[test]
    fn parse_fuzzy_with_criteria() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `true`
  expect: exit_code 0
### Fuzzy
- Code is well-structured and readable
- Error messages are helpful to users
- Edge cases are handled gracefully
"#;
        let criteria = parse_fuzzy_criteria(body);
        assert_eq!(criteria.len(), 3);
        assert_eq!(criteria[0], "Code is well-structured and readable");
        assert_eq!(criteria[1], "Error messages are helpful to users");
        assert_eq!(criteria[2], "Edge cases are handled gracefully");
    }

    #[test]
    fn parse_fuzzy_stops_at_next_heading() {
        let body = r#"## Acceptance Criteria
### Fuzzy
- Quality check one
- Quality check two
### Deterministic
- command: `true`
  expect: exit_code 0
"#;
        let criteria = parse_fuzzy_criteria(body);
        assert_eq!(criteria.len(), 2);
    }

    #[test]
    fn parse_json_path_criterion() {
        let body = r#"## Acceptance Criteria
### Deterministic
- command: `cat /tmp/data.json`
  expect: json_path "$.name" "apex"
"#;
        let criteria = parse_criteria(body);
        assert_eq!(criteria.len(), 1);
        assert!(matches!(&criteria[0].check, CheckType::JsonPath { .. }));
    }
}
