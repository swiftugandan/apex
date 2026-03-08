use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use serde_json::json;

pub fn definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "file_read".into(),
            description: "Read the contents of a file".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read" },
                    "max_bytes": { "type": "integer", "description": "Maximum bytes to read (default 65536)" }
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

    let max_bytes = call.input["max_bytes"].as_u64().unwrap_or(65536) as usize;

    match tokio::fs::read(path).await {
        Ok(data) => {
            let truncated = data.len() > max_bytes;
            let bytes_read = data.len().min(max_bytes);
            let content = String::from_utf8_lossy(&data[..bytes_read]);

            Ok(ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                output: json!({
                    "path": path,
                    "content": content,
                    "bytes_read": bytes_read,
                    "truncated": truncated,
                }),
                is_error: false,
                truncated,
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
        assert_eq!(result.output["bytes_read"], 11);
        assert!(!result.output["truncated"].as_bool().unwrap());

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
}
