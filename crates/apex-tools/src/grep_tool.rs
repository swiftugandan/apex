use std::path::{Path, PathBuf};

use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use regex::RegexBuilder;
use serde_json::json;

use crate::tool_result_helpers::ok_result;

pub fn definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "grep".into(),
            description: "Search file contents using regex. Three output modes: 'files_with_matches' returns just file paths (default, fast), 'content' returns matching lines with optional context, 'count' returns match counts per file. Use the glob parameter to filter which files to search.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for in file contents" },
                    "path": { "type": "string", "description": "File or directory to search in (default: current working directory)" },
                    "glob": { "type": "string", "description": "Glob pattern to filter which files to search (e.g. '*.rs', '*.{ts,tsx}')" },
                    "output_mode": { "type": "string", "enum": ["files_with_matches", "content", "count"], "description": "Output mode (default: files_with_matches)" },
                    "context": { "type": "integer", "description": "Number of lines to show before and after each match (only for content mode)" },
                    "case_insensitive": { "type": "boolean", "description": "Case insensitive search (default false)" },
                    "max_results": { "type": "integer", "description": "Maximum results to return (default 100)" }
                },
                "required": ["pattern"]
            }),
        },
    }
}

pub async fn execute(call: &ToolCall) -> Result<ToolResult, ToolError> {
    let pattern_str = call.input["pattern"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput("missing 'pattern' field".into()))?;

    let path = call.input["path"].as_str().unwrap_or(".");
    let glob_filter = call.input["glob"].as_str();
    let output_mode = call.input["output_mode"]
        .as_str()
        .unwrap_or("files_with_matches");
    let context_lines = call.input["context"].as_u64().unwrap_or(0) as usize;
    let case_insensitive = call.input["case_insensitive"].as_bool().unwrap_or(false);
    let max_results = call.input["max_results"].as_u64().unwrap_or(100) as usize;

    let regex = RegexBuilder::new(pattern_str)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| ToolError::InvalidInput(format!("invalid regex: {e}")))?;

    let glob_matcher = glob_filter
        .map(glob::Pattern::new)
        .transpose()
        .map_err(|e| ToolError::InvalidInput(format!("invalid glob filter: {e}")))?;

    let files = collect_files(path, &glob_matcher);

    match output_mode {
        "files_with_matches" => {
            let mut matched_files: Vec<String> = Vec::new();
            for file in &files {
                if matched_files.len() >= max_results {
                    break;
                }
                if let Ok(content) = tokio::fs::read_to_string(file).await {
                    if regex.is_match(&content) {
                        matched_files.push(file.to_string_lossy().to_string());
                    }
                }
            }
            ok_result(
                call,
                json!({
                    "pattern": pattern_str,
                    "matches": matched_files,
                    "total": matched_files.len(),
                }),
            )
        }
        "content" => {
            let mut results: Vec<serde_json::Value> = Vec::new();
            for file in &files {
                if results.len() >= max_results {
                    break;
                }
                if let Ok(content) = tokio::fs::read_to_string(file).await {
                    let lines: Vec<&str> = content.lines().collect();
                    for (i, line) in lines.iter().enumerate() {
                        if results.len() >= max_results {
                            break;
                        }
                        if regex.is_match(line) {
                            let start = i.saturating_sub(context_lines);
                            let end = (i + context_lines + 1).min(lines.len());
                            let context_before = if start < i {
                                lines[start..i].join("\n")
                            } else {
                                String::new()
                            };
                            let context_after = if i + 1 < end {
                                lines[i + 1..end].join("\n")
                            } else {
                                String::new()
                            };
                            results.push(json!({
                                "file": file.to_string_lossy(),
                                "line_number": i + 1,
                                "line": line,
                                "context_before": context_before,
                                "context_after": context_after,
                            }));
                        }
                    }
                }
            }
            ok_result(
                call,
                json!({
                    "pattern": pattern_str,
                    "matches": results,
                    "total": results.len(),
                }),
            )
        }
        "count" => {
            let mut counts: Vec<serde_json::Value> = Vec::new();
            for file in &files {
                if counts.len() >= max_results {
                    break;
                }
                if let Ok(content) = tokio::fs::read_to_string(file).await {
                    let count = regex.find_iter(&content).count();
                    if count > 0 {
                        counts.push(json!({
                            "file": file.to_string_lossy(),
                            "count": count,
                        }));
                    }
                }
            }
            ok_result(
                call,
                json!({
                    "pattern": pattern_str,
                    "matches": counts,
                    "total": counts.len(),
                }),
            )
        }
        _ => Err(ToolError::InvalidInput(format!(
            "unknown output_mode: {output_mode}. Expected 'files_with_matches', 'content', or 'count'"
        ))),
    }
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

fn collect_files(path: &str, glob_matcher: &Option<glob::Pattern>) -> Vec<PathBuf> {
    let p = Path::new(path);
    if p.is_file() {
        return vec![p.to_path_buf()];
    }

    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(path).into_iter();
    for entry in walker
        .filter_entry(|e| {
            // Skip hidden directories (but allow the root)
            if e.depth() > 0 {
                if let Some(name) = e.file_name().to_str() {
                    return !is_hidden(name);
                }
            }
            true
        })
        .flatten()
    {
        if entry.file_type().is_file() {
            if let Some(ref g) = glob_matcher {
                if let Some(name) = entry.file_name().to_str() {
                    if !g.matches(name) {
                        continue;
                    }
                }
            }
            files.push(entry.into_path());
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_call(input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "test-id".into(),
            name: "grep".into(),
            input,
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/apex-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn files_with_matches_basic() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "hello world").unwrap();
        std::fs::write(dir.join("b.txt"), "goodbye world").unwrap();
        std::fs::write(dir.join("c.txt"), "no match here").unwrap();

        let call = make_call(json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].as_str().unwrap().contains("a.txt"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn content_mode_with_line_numbers() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "line1\nfind me\nline3").unwrap();

        let call = make_call(json!({
            "pattern": "find me",
            "path": dir.to_str().unwrap(),
            "output_mode": "content"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line_number"], 2);
        assert_eq!(matches[0]["line"], "find me");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn content_mode_with_context() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "line1\nline2\ntarget\nline4\nline5").unwrap();

        let call = make_call(json!({
            "pattern": "target",
            "path": dir.to_str().unwrap(),
            "output_mode": "content",
            "context": 1
        }));
        let result = execute(&call).await.unwrap();

        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["context_before"], "line2");
        assert_eq!(matches[0]["context_after"], "line4");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn count_mode() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "foo bar foo").unwrap();
        std::fs::write(dir.join("b.txt"), "foo").unwrap();

        let call = make_call(json!({
            "pattern": "foo",
            "path": dir.to_str().unwrap(),
            "output_mode": "count"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        // Find the file with 2 counts
        let total_count: u64 = matches.iter().map(|m| m["count"].as_u64().unwrap()).sum();
        assert_eq!(total_count, 3);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn glob_filter() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.rs"), "hello").unwrap();
        std::fs::write(dir.join("b.txt"), "hello").unwrap();

        let call = make_call(json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap(),
            "glob": "*.rs"
        }));
        let result = execute(&call).await.unwrap();

        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].as_str().unwrap().contains("a.rs"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn case_insensitive() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "Hello World").unwrap();

        let call = make_call(json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap(),
            "case_insensitive": true
        }));
        let result = execute(&call).await.unwrap();

        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn invalid_regex() {
        let call = make_call(json!({ "pattern": "[invalid" }));
        let err = execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn no_matches() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();

        let call = make_call(json!({
            "pattern": "zzzzz",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let matches = result.output["matches"].as_array().unwrap();
        assert!(matches.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn max_results_respected() {
        let dir = temp_dir();
        for i in 0..10 {
            std::fs::write(dir.join(format!("f{i}.txt")), "match me").unwrap();
        }

        let call = make_call(json!({
            "pattern": "match",
            "path": dir.to_str().unwrap(),
            "max_results": 3
        }));
        let result = execute(&call).await.unwrap();

        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 3);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn skips_hidden_dirs() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join(".hidden/secret.txt"), "match").unwrap();
        std::fs::write(dir.join("visible.txt"), "match").unwrap();

        let call = make_call(json!({
            "pattern": "match",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].as_str().unwrap().contains("visible.txt"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn single_file_search() {
        let dir = temp_dir();
        let path = dir.join("target.txt");
        std::fs::write(&path, "find this line\nand this").unwrap();

        let call = make_call(json!({
            "pattern": "find this",
            "path": path.to_str().unwrap(),
            "output_mode": "content"
        }));
        let result = execute(&call).await.unwrap();

        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line_number"], 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let dir = temp_dir();
        std::fs::write(dir.join("binary.bin"), &[0u8, 1, 2, 0xFF, 0xFE]).unwrap();
        std::fs::write(dir.join("text.txt"), "hello").unwrap();

        let call = make_call(json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap()
        }));
        // Should not crash
        let result = execute(&call).await.unwrap();
        assert!(!result.is_error);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
