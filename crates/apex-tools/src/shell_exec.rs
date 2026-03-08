use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use serde_json::json;
use tokio::process::Command;

pub fn definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "shell_exec".into(),
            description: "Run a shell command via /bin/sh -c".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "cwd": { "type": "string", "description": "Working directory (optional)" },
                    "max_output": { "type": "integer", "description": "Max output bytes (default 16384)" }
                },
                "required": ["command"]
            }),
        },
    }
}

pub async fn execute(call: &ToolCall) -> Result<ToolResult, ToolError> {
    let command = call.input["command"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput("missing 'command' field".into()))?;

    let max_output = call.input["max_output"].as_u64().unwrap_or(16384) as usize;

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);

    if let Some(cwd) = call.input["cwd"].as_str() {
        cmd.current_dir(cwd);
    }

    let result = cmd.output().await;

    match result {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(-1);

            let stdout_bytes = &output.stdout;
            let stderr_bytes = &output.stderr;

            let stdout_truncated = stdout_bytes.len() > max_output;
            let stderr_truncated = stderr_bytes.len() > max_output;

            let stdout =
                String::from_utf8_lossy(&stdout_bytes[..stdout_bytes.len().min(max_output)]);
            let stderr =
                String::from_utf8_lossy(&stderr_bytes[..stderr_bytes.len().min(max_output)]);

            Ok(ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                output: json!({
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                    "truncated": stdout_truncated || stderr_truncated,
                }),
                is_error: false,
            })
        }
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
            name: "shell_exec".into(),
            input,
        }
    }

    #[tokio::test]
    async fn simple_echo() {
        let call = make_call(json!({"command": "echo hello"}));
        let result = execute(&call).await.unwrap();
        assert_eq!(result.output["exit_code"], 0);
        assert!(result.output["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(result.output["stderr"], "");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn nonzero_exit() {
        let call = make_call(json!({"command": "exit 42"}));
        let result = execute(&call).await.unwrap();
        assert_eq!(result.output["exit_code"], 42);
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn stderr_output() {
        let call = make_call(json!({"command": "echo err >&2"}));
        let result = execute(&call).await.unwrap();
        assert!(result.output["stderr"].as_str().unwrap().contains("err"));
    }

    #[tokio::test]
    async fn cwd() {
        let call = make_call(json!({"command": "pwd", "cwd": "/tmp"}));
        let result = execute(&call).await.unwrap();
        assert!(result.output["stdout"].as_str().unwrap().contains("/tmp"));
    }

    #[tokio::test]
    async fn missing_command_field() {
        let call = make_call(json!({}));
        let err = execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn truncation() {
        let call = make_call(json!({"command": "seq 1 10000", "max_output": 32}));
        let result = execute(&call).await.unwrap();
        assert!(result.output["truncated"].as_bool().unwrap());
        assert!(result.output["stdout"].as_str().unwrap().len() <= 32);
    }
}
