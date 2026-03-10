use std::path::PathBuf;
use std::time::SystemTime;

use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use serde_json::json;

use crate::tool_result_helpers::ok_result;

pub fn definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "glob".into(),
            description: "Find files matching a glob pattern. Returns file paths sorted by modification time (most recent first). Use this to discover files by name pattern before reading or editing them.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern to match files (e.g. '**/*.rs', 'src/**/*.ts')" },
                    "path": { "type": "string", "description": "Base directory to search in (default: current working directory)" },
                    "max_results": { "type": "integer", "description": "Maximum number of results to return (default 500)" }
                },
                "required": ["pattern"]
            }),
        },
    }
}

pub async fn execute(call: &ToolCall) -> Result<ToolResult, ToolError> {
    let pattern = call.input["pattern"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput("missing 'pattern' field".into()))?;

    let base = call.input["path"].as_str().unwrap_or(".");
    let max_results = call.input["max_results"].as_u64().unwrap_or(500) as usize;

    let full_pattern = if pattern.starts_with('/') {
        pattern.to_string()
    } else {
        format!("{base}/{pattern}")
    };

    let entries = glob::glob(&full_pattern)
        .map_err(|e| ToolError::InvalidInput(format!("invalid glob pattern: {e}")))?;

    let mut results: Vec<(PathBuf, SystemTime)> = Vec::new();
    for entry in entries {
        if let Ok(path) = entry {
            // Single metadata() call to check file type and get mtime.
            if let Ok(meta) = path.metadata() {
                if meta.is_file() {
                    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    results.push((path, mtime));
                }
            }
        }
    }

    let total = results.len();

    // Sort by mtime descending (most recently modified first)
    results.sort_by(|a, b| b.1.cmp(&a.1));

    let truncated = total > max_results;
    results.truncate(max_results);

    let matches: Vec<String> = results
        .iter()
        .map(|(p, _)| p.to_string_lossy().to_string())
        .collect();

    ok_result(
        call,
        json!({
            "pattern": pattern,
            "base_path": base,
            "matches": matches,
            "total_matches": total,
            "truncated": truncated,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_call(input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "test-id".into(),
            name: "glob".into(),
            input,
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/apex-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn matches_files_in_directory() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        std::fs::write(dir.join("c.rs"), "c").unwrap();

        let call = make_call(json!({
            "pattern": "*.txt",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(result.output["total_matches"], 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn recursive_glob() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "b").unwrap();

        let call = make_call(json!({
            "pattern": "**/*.txt",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn no_matches() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "a").unwrap();

        let call = make_call(json!({
            "pattern": "*.rs",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let matches = result.output["matches"].as_array().unwrap();
        assert!(matches.is_empty());
        assert_eq!(result.output["total_matches"], 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn invalid_pattern() {
        let call = make_call(json!({ "pattern": "[invalid" }));
        let err = execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn respects_max_results() {
        let dir = temp_dir();
        for i in 0..10 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }

        let call = make_call(json!({
            "pattern": "*.txt",
            "path": dir.to_str().unwrap(),
            "max_results": 3
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(result.output["total_matches"], 10);
        assert!(result.output["truncated"].as_bool().unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn sorted_by_mtime() {
        let dir = temp_dir();
        std::fs::write(dir.join("old.txt"), "old").unwrap();
        // Small delay so mtime differs
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.join("new.txt"), "new").unwrap();

        let call = make_call(json!({
            "pattern": "*.txt",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        let matches = result.output["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        let first = matches[0].as_str().unwrap();
        assert!(first.contains("new.txt"), "most recent file should be first");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn excludes_directories() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("file.txt"), "f").unwrap();

        let call = make_call(json!({
            "pattern": "*",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        let matches = result.output["matches"].as_array().unwrap();
        for m in matches {
            assert!(!m.as_str().unwrap().ends_with("subdir"));
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn custom_base_path() {
        let dir = temp_dir();
        std::fs::write(dir.join("test.txt"), "t").unwrap();

        let call = make_call(json!({
            "pattern": "*.txt",
            "path": dir.to_str().unwrap()
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(result.output["base_path"], dir.to_str().unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
