use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use serde_json::json;
use tokio::fs;
use tokio::io::AsyncWriteExt;

pub fn definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "file_write".into(),
            description: "Write content to a file".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to write" },
                    "content": { "type": "string", "description": "Content to write" },
                    "append": { "type": "boolean", "description": "Append instead of overwrite (default false)" }
                },
                "required": ["path", "content"]
            }),
        },
    }
}

pub async fn execute(call: &ToolCall) -> Result<ToolResult, ToolError> {
    let path = call.input["path"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput("missing 'path' field".into()))?;

    let content = call.input["content"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput("missing 'content' field".into()))?;

    let append = call.input["append"].as_bool().unwrap_or(false);

    let write_result = async {
        if let Some(parent) = std::path::Path::new(path).parent() {
            fs::create_dir_all(parent).await?;
        }

        if append {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await?;
            file.write_all(content.as_bytes()).await?;
        } else {
            fs::write(path, content.as_bytes()).await?;
        }

        Ok::<_, std::io::Error>(())
    }
    .await;

    match write_result {
        Ok(()) => Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({
                "path": path,
                "bytes_written": content.len(),
                "appended": append,
            }),
            is_error: false,
        }),
        Err(e) => Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "error": e.to_string() }),
            is_error: true,
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
            name: "file_write".into(),
            input,
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/apex-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn write_new_file() {
        let dir = temp_dir();
        let path = dir.join("out.txt");

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "content": "hello"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn append() {
        let dir = temp_dir();
        let path = dir.join("append.txt");

        let write_call = make_call(json!({
            "path": path.to_str().unwrap(),
            "content": "first"
        }));
        execute(&write_call).await.unwrap();

        let append_call = make_call(json!({
            "path": path.to_str().unwrap(),
            "content": "second",
            "append": true
        }));
        let result = execute(&append_call).await.unwrap();

        assert!(!result.is_error);
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "firstsecond");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = temp_dir();
        let path = dir.join("sub/dir/file.txt");

        let call = make_call(json!({
            "path": path.to_str().unwrap(),
            "content": "nested"
        }));
        let result = execute(&call).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "nested");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn missing_path() {
        let call = make_call(json!({"content": "hello"}));
        let err = execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn missing_content() {
        let call = make_call(json!({"path": "/tmp/whatever.txt"}));
        let err = execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
