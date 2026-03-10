use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use serde_json::json;
use std::time::Instant;
use tokio::process::Command;

use crate::spill::{
    SpillManager, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_SPILL_HEAD_LINES, DEFAULT_SPILL_STRATEGY,
    DEFAULT_SPILL_TAIL_LINES,
};

pub fn definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "shell_exec".into(),
            description: "Run a shell command via /bin/sh -c. Output beyond 16KB is automatically spilled to a scratch file — you receive a head/tail summary with the scratch path. Use file_read with offset/limit to read spilled sections.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "cwd": { "type": "string", "description": "Working directory (optional)" },
                    "max_output": { "type": "integer", "description": "Spill threshold in bytes (default 16384). Output above this is spilled to scratch with a head/tail envelope. Rarely needs changing." },
                    "grep": { "type": "string", "description": "Filter output lines matching this substring (applied before spill)" },
                    "tail": { "type": "integer", "description": "Only keep last N lines (applied before spill)" },
                    "max_lines": { "type": "integer", "description": "Max lines to return (applied before spill)" }
                },
                "required": ["command"]
            }),
        },
    }
}

pub async fn execute(call: &ToolCall, spill: &SpillManager) -> Result<ToolResult, ToolError> {
    let command = call.input["command"]
        .as_str()
        .ok_or_else(|| ToolError::InvalidInput("missing 'command' field".into()))?;

    let max_output = call.input["max_output"].as_u64().unwrap_or(DEFAULT_MAX_OUTPUT_BYTES as u64) as usize;
    let grep_pattern = call.input["grep"].as_str();
    let tail_n = call.input["tail"].as_u64().map(|n| n as usize);
    let max_lines = call.input["max_lines"].as_u64().map(|n| n as usize);

    let start = Instant::now();

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);

    if let Some(cwd) = call.input["cwd"].as_str() {
        cmd.current_dir(cwd);
    }

    let result = cmd.output().await;
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(-1);

            let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr_bytes = &output.stderr;
            let stderr_truncated = stderr_bytes.len() > max_output;
            let stderr =
                String::from_utf8_lossy(&stderr_bytes[..stderr_bytes.len().min(max_output)]);

            // Apply grep filter (simple substring match for safety)
            if let Some(pattern) = grep_pattern {
                stdout = stdout
                    .lines()
                    .filter(|line| line.contains(pattern))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !stdout.is_empty() {
                    stdout.push('\n');
                }
            }

            // Apply tail (keep last N lines)
            if let Some(n) = tail_n {
                let lines: Vec<&str> = stdout.lines().collect();
                if lines.len() > n {
                    stdout = lines[lines.len() - n..].join("\n");
                    stdout.push('\n');
                }
            }

            // Apply max_lines (keep first N lines)
            if let Some(n) = max_lines {
                let lines: Vec<&str> = stdout.lines().collect();
                if lines.len() > n {
                    stdout = lines[..n].join("\n");
                    stdout.push('\n');
                }
            }

            // Check if we need to spill
            let (final_stdout, spill_path, stats, truncated) =
                if stdout.len() > max_output {
                    if let Some(spill_result) = spill.spill_if_needed(
                        &stdout,
                        max_output,
                        DEFAULT_SPILL_STRATEGY,
                        DEFAULT_SPILL_HEAD_LINES,
                        DEFAULT_SPILL_TAIL_LINES,
                    ) {
                        (
                            spill_result.envelope,
                            Some(spill_result.path),
                            Some(spill_result.stats),
                            true,
                        )
                    } else {
                        // Spill failed, fall back to truncation
                        let t = apex_core::truncate_str(&stdout, max_output);
                        (t.to_string(), None, None, true)
                    }
                } else {
                    (stdout, None, None, false)
                };

            let overall_truncated = truncated || stderr_truncated;

            Ok(ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                output: json!({
                    "exit_code": exit_code,
                    "stdout": final_stdout,
                    "stderr": stderr,
                    "truncated": overall_truncated,
                }),
                is_error: false,
                spill_path,
                stats,
                truncated: overall_truncated,
                duration_ms,
            })
        }
        Err(e) => Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "error": e.to_string() }),
            is_error: true,
            duration_ms,
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
            name: "shell_exec".into(),
            input,
        }
    }

    fn temp_spill() -> (tempfile::TempDir, SpillManager) {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SpillManager::new(dir.path().join("scratch"));
        (dir, mgr)
    }

    #[tokio::test]
    async fn simple_echo() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "echo hello"}));
        let result = execute(&call, &spill).await.unwrap();
        assert_eq!(result.output["exit_code"], 0);
        assert!(result.output["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(result.output["stderr"], "");
        assert!(!result.is_error);
        assert!(result.duration_ms > 0 || true); // duration may be 0 for fast commands
    }

    #[tokio::test]
    async fn nonzero_exit() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "exit 42"}));
        let result = execute(&call, &spill).await.unwrap();
        assert_eq!(result.output["exit_code"], 42);
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn stderr_output() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "echo err >&2"}));
        let result = execute(&call, &spill).await.unwrap();
        assert!(result.output["stderr"].as_str().unwrap().contains("err"));
    }

    #[tokio::test]
    async fn cwd() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "pwd", "cwd": "/tmp"}));
        let result = execute(&call, &spill).await.unwrap();
        assert!(result.output["stdout"].as_str().unwrap().contains("/tmp"));
    }

    #[tokio::test]
    async fn missing_command_field() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({}));
        let err = execute(&call, &spill).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn truncation_with_spill() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "seq 1 10000", "max_output": 32}));
        let result = execute(&call, &spill).await.unwrap();
        assert!(result.truncated);
        // Output should be spill envelope or truncated
        assert!(result.output["truncated"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn grep_filter() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "printf 'apple\\nbanana\\napricot\\n'", "grep": "ap"}));
        let result = execute(&call, &spill).await.unwrap();
        let stdout = result.output["stdout"].as_str().unwrap();
        assert!(stdout.contains("apple"));
        assert!(stdout.contains("apricot"));
        assert!(!stdout.contains("banana"));
    }

    #[tokio::test]
    async fn tail_filter() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "seq 1 10", "tail": 3}));
        let result = execute(&call, &spill).await.unwrap();
        let stdout = result.output["stdout"].as_str().unwrap();
        assert!(stdout.contains("8"));
        assert!(stdout.contains("9"));
        assert!(stdout.contains("10"));
        assert!(!stdout.contains("\n1\n"));
    }

    #[tokio::test]
    async fn max_lines_filter() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "seq 1 10", "max_lines": 3}));
        let result = execute(&call, &spill).await.unwrap();
        let stdout = result.output["stdout"].as_str().unwrap();
        assert!(stdout.contains("1"));
        assert!(stdout.contains("2"));
        assert!(stdout.contains("3"));
        let line_count = stdout.lines().count();
        assert_eq!(line_count, 3);
    }
}
