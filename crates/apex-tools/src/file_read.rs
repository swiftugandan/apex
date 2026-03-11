use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use serde_json::json;

pub fn definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "file_read".into(),
            description: "Read the contents of a file. Supports line-based offset and limit for reading specific ranges without loading the entire file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read" },
                    "offset": { "type": "integer", "description": "Start reading from this line number (1-based, default 1)" },
                    "limit": { "type": "integer", "description": "Maximum number of lines to return (default: all lines up to max_bytes)" },
                    "max_bytes": { "type": "integer", "description": "Maximum bytes to return (default 65536). Applied after offset/limit." }
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

    let offset = call.input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = call.input["limit"].as_u64().map(|n| n as usize);
    let max_bytes = call.input["max_bytes"].as_u64().unwrap_or(65536) as usize;

    match tokio::fs::read(path).await {
        Ok(data) => {
            let full_text = String::from_utf8_lossy(&data);
            let total_lines = full_text.lines().count();

            // Apply line-based offset and limit
            let selected: String = {
                let skip = offset.saturating_sub(1);
                let iter = full_text.lines().skip(skip);
                let taken: Vec<&str> = match limit {
                    Some(n) => iter.take(n).collect(),
                    None => iter.collect(),
                };
                taken.join("\n")
            };

            let truncated_bytes = selected.len() > max_bytes;
            let content = apex_core::truncate_str(&selected, max_bytes);
            let bytes_read = content.len();

            let lines_returned = content.lines().count();
            let has_offset_or_limit = offset > 1 || limit.is_some();

            let mut output = json!({
                "path": path,
                "content": content,
                "bytes_read": bytes_read,
                "truncated": truncated_bytes,
                "total_lines": total_lines,
                "lines_returned": lines_returned,
            });

            if has_offset_or_limit {
                output["from_line"] = json!(offset);
                output["to_line"] = json!(offset + lines_returned.saturating_sub(1));
            }

            Ok(ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                output,
                is_error: false,
                truncated: truncated_bytes,
                ..Default::default()
            })
        }
        Err(e) => Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "error": e.to_string() }),
            is_error: true,
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_call(input: serde_json::Value) -> apex_core::domain::ToolCall {
        apex_core::domain::ToolCall {
            id: "test-id".into(),
            name: "file_read".into(),
            input,
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/apex-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn read_existing_file() {
        let dir = temp_dir();
        let path = dir.join("hello.txt");
        std::fs::write(&path, "hello world").unwrap();

        let call = make_call(json!({"path": path.to_str().unwrap()}));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(result.output["content"], "hello world");
        assert_eq!(result.output["total_lines"], 1);
        assert!(!result.output["truncated"].as_bool().unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let dir = temp_dir();
        let path = dir.join("lines.txt");
        let content = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        // Read lines 5-9 (offset=5, limit=5)
        let call = make_call(json!({"path": path.to_str().unwrap(), "offset": 5, "limit": 5}));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        let out = result.output["content"].as_str().unwrap();
        assert!(out.starts_with("line 5"));
        assert!(out.contains("line 9"));
        assert!(!out.contains("line 4"));
        assert!(!out.contains("line 10"));
        assert_eq!(result.output["from_line"], 5);
        assert_eq!(result.output["to_line"], 9);
        assert_eq!(result.output["total_lines"], 20);
        assert_eq!(result.output["lines_returned"], 5);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn read_with_offset_only() {
        let dir = temp_dir();
        let path = dir.join("lines.txt");
        let content = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &content).unwrap();

        // Read from line 8 onward
        let call = make_call(json!({"path": path.to_str().unwrap(), "offset": 8}));
        let result = execute(&call).await.unwrap();

        let out = result.output["content"].as_str().unwrap();
        assert!(out.starts_with("line 8"));
        assert!(out.contains("line 10"));
        assert!(!out.contains("line 7"));
        assert_eq!(result.output["lines_returned"], 3);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn truncation() {
        let dir = temp_dir();
        let path = dir.join("big.txt");
        std::fs::write(&path, "a".repeat(200)).unwrap();

        let call = make_call(json!({"path": path.to_str().unwrap(), "max_bytes": 50}));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert!(result.output["truncated"].as_bool().unwrap());
        assert_eq!(result.output["bytes_read"], 50);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn nonexistent_file() {
        let call = make_call(json!({"path": "/tmp/apex-test-nonexistent-xxxxx"}));
        let result = execute(&call).await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn missing_path_field() {
        let call = make_call(json!({}));
        let err = execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn read_multibyte_truncation() {
        let dir = temp_dir();
        let path = dir.join("emoji.txt");
        // Each emoji is 4 bytes
        let content = "😀😁😂🤣😃😄😅😆😇😈";
        std::fs::write(&path, content).unwrap();

        // max_bytes=5 lands in the middle of the second emoji (byte 5 of 40)
        let call = make_call(json!({"path": path.to_str().unwrap(), "max_bytes": 5}));
        let result = execute(&call).await.unwrap();

        // Should not panic and should truncate to valid UTF-8
        assert!(!result.is_error);
        assert!(result.output["truncated"].as_bool().unwrap());
        // Should have truncated to 4 bytes (one complete emoji)
        assert_eq!(result.output["bytes_read"], 4);
        let out = result.output["content"].as_str().unwrap();
        assert_eq!(out, "😀");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
