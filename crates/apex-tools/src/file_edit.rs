use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use serde_json::json;

use crate::tool_result_helpers::{err_result, ok_result};

pub fn definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "file_edit".into(),
            description: "Edit a file using exact string replacement (str_replace) or line insertion (insert). For str_replace: provide old_string and new_string — old_string must match exactly one location in the file unless replace_all is true. For insert: provide insert_line and new_string to insert text after a specific line. Use file_read first to see the current content before editing.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit" },
                    "command": { "type": "string", "enum": ["str_replace", "insert"], "description": "Edit command (default: str_replace)" },
                    "old_string": { "type": "string", "description": "Exact text to find (required for str_replace)" },
                    "new_string": { "type": "string", "description": "Replacement text (required for str_replace and insert)" },
                    "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false, str_replace only)" },
                    "insert_line": { "type": "integer", "description": "Insert new_string AFTER this line number, 1-indexed (required for insert)" }
                },
                "required": ["path"]
            }),
        },
    }
}

pub async fn execute(call: &ToolCall) -> Result<ToolResult, ToolError> {
    let path = call.input["path"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput("missing 'path' field".into()))?;

    let command = call.input["command"].as_str().unwrap_or("str_replace");

    match command {
        "str_replace" => execute_str_replace(call, path).await,
        "insert" => execute_insert(call, path).await,
        _ => Err(ToolError::InvalidInput(format!(
            "unknown command: {command}. Expected 'str_replace' or 'insert'"
        ))),
    }
}

async fn execute_str_replace(call: &ToolCall, path: &str) -> Result<ToolResult, ToolError> {
    let old_string = call.input["old_string"].as_str().ok_or_else(|| {
        ToolError::InvalidInput("missing 'old_string' field for str_replace".into())
    })?;

    let new_string = call.input["new_string"].as_str().ok_or_else(|| {
        ToolError::InvalidInput("missing 'new_string' field for str_replace".into())
    })?;

    let replace_all = call.input["replace_all"].as_bool().unwrap_or(false);

    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) => return err_result(call, &format!("failed to read {path}: {e}")),
    };

    // Count occurrences, but short-circuit after 2 when replace_all is false.
    let count = if replace_all {
        content.matches(old_string).count()
    } else {
        content.matches(old_string).take(2).count()
    };

    if count == 0 {
        return err_result(
            call,
            &format!("old_string not found in {path}. Use file_read to verify the exact content."),
        );
    }

    if !replace_all && count > 1 {
        return err_result(
            call,
            &format!(
                "old_string matches multiple times in {path}. Provide more surrounding context to make it unique, or set replace_all=true."
            ),
        );
    }

    let new_content = if replace_all {
        content.replace(old_string, new_string)
    } else {
        content.replacen(old_string, new_string, 1)
    };

    if let Err(e) = tokio::fs::write(path, &new_content).await {
        return err_result(call, &format!("failed to write {path}: {e}"));
    }

    ok_result(
        call,
        json!({
            "path": path,
            "command": "str_replace",
            "replacements": if replace_all { count } else { 1 },
        }),
    )
}

async fn execute_insert(call: &ToolCall, path: &str) -> Result<ToolResult, ToolError> {
    let insert_line = call.input["insert_line"]
        .as_u64()
        .ok_or_else(|| ToolError::InvalidInput("missing 'insert_line' field for insert".into()))?
        as usize;

    let new_string = call.input["new_string"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput("missing 'new_string' field for insert".into()))?;

    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) => return err_result(call, &format!("failed to read {path}: {e}")),
    };

    let trailing_newline = content.ends_with('\n');
    let mut lines: Vec<&str> = content.lines().collect();

    if insert_line > lines.len() {
        return err_result(
            call,
            &format!(
                "insert_line {insert_line} exceeds file length ({} lines)",
                lines.len()
            ),
        );
    }

    lines.insert(insert_line, new_string);
    let mut new_content = lines.join("\n");
    if trailing_newline {
        new_content.push('\n');
    }

    if let Err(e) = tokio::fs::write(path, &new_content).await {
        return err_result(call, &format!("failed to write {path}: {e}"));
    }

    ok_result(
        call,
        json!({
            "path": path,
            "command": "insert",
            "insert_after_line": insert_line,
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
            name: "file_edit".into(),
            input,
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/apex-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn str_replace_single_match() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "old_string": "world",
            "new_string": "rust"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(result.output["replacements"], 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello rust");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn str_replace_not_found() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "old_string": "missing",
            "new_string": "replacement"
        }));
        let result = execute(&call).await.unwrap();

        assert!(result.is_error);
        let err = result.output["error"].as_str().unwrap();
        assert!(err.contains("not found"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn str_replace_multiple_matches_fails() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "old_string": "aaa",
            "new_string": "ccc"
        }));
        let result = execute(&call).await.unwrap();

        assert!(result.is_error);
        let err = result.output["error"].as_str().unwrap();
        assert!(err.contains("multiple times"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn str_replace_replace_all() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "old_string": "aaa",
            "new_string": "ccc",
            "replace_all": true
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(result.output["replacements"], 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ccc bbb ccc");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn str_replace_replace_all_zero_matches() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "hello world").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "old_string": "missing",
            "new_string": "replacement",
            "replace_all": true
        }));
        let result = execute(&call).await.unwrap();

        assert!(result.is_error);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn insert_after_line() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "command": "insert",
            "insert_line": 2,
            "new_string": "inserted"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\nline2\ninserted\nline3"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn insert_at_end() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "command": "insert",
            "insert_line": 2,
            "new_string": "line3"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\nline2\nline3"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn insert_line_out_of_range() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "command": "insert",
            "insert_line": 10,
            "new_string": "nope"
        }));
        let result = execute(&call).await.unwrap();

        assert!(result.is_error);
        let err = result.output["error"].as_str().unwrap();
        assert!(err.contains("exceeds file length"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn missing_path() {
        let call = make_call(json!({ "old_string": "a", "new_string": "b" }));
        let err = execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn preserves_trailing_newline() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2\n").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "old_string": "line1",
            "new_string": "replaced"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.ends_with('\n'));
        assert_eq!(content, "replaced\nline2\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn insert_at_zero_prepends() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "command": "insert",
            "insert_line": 0,
            "new_string": "prepended"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "prepended\nline1\nline2"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn insert_at_one_after_first() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "command": "insert",
            "insert_line": 1,
            "new_string": "inserted"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\ninserted\nline2\nline3"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn insert_at_end_appends() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "command": "insert",
            "insert_line": 3,
            "new_string": "appended"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\nline2\nline3\nappended"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn insert_beyond_length_errors() {
        let dir = temp_dir();
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2").unwrap();

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "command": "insert",
            "insert_line": 5,
            "new_string": "nope"
        }));
        let result = execute(&call).await.unwrap();

        assert!(result.is_error);
        let err = result.output["error"].as_str().unwrap();
        assert!(err.contains("exceeds file length"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
