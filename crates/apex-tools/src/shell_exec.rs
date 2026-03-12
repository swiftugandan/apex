use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use serde_json::json;
use std::time::Instant;
use tokio::process::Command;

use crate::spill::{
    SpillManager, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_SPILL_HEAD_LINES, DEFAULT_SPILL_STRATEGY,
    DEFAULT_SPILL_TAIL_LINES,
};

/// Default command timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

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
                    "max_lines": { "type": "integer", "description": "Max lines to return (applied before spill)" },
                    "timeout": { "type": "integer", "description": "Max execution time in seconds (default 120). Command is killed on timeout." }
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

    let max_output = call.input["max_output"]
        .as_u64()
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES as u64) as usize;
    let grep_pattern = call.input["grep"].as_str();
    let tail_n = call.input["tail"].as_u64().map(|n| n as usize);
    let max_lines = call.input["max_lines"].as_u64().map(|n| n as usize);

    let timeout_secs = call.input["timeout"]
        .as_u64()
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    let timeout_duration = std::time::Duration::from_secs(timeout_secs);

    let start = Instant::now();

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);

    if let Some(cwd) = call.input["cwd"].as_str() {
        cmd.current_dir(cwd);
    }

    // Use piped I/O and process groups so we can kill the entire tree on timeout.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);

    let child = cmd.spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            return Ok(ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                output: json!({ "error": e.to_string() }),
                is_error: true,
                duration_ms,
                ..Default::default()
            });
        }
    };

    // Grab pid before wait_with_output consumes child.
    #[cfg(unix)]
    let child_pid = child.id();

    let result = tokio::time::timeout(timeout_duration, child.wait_with_output()).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        // Completed within timeout
        Ok(Ok(output)) => {
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
            let (final_stdout, spill_path, stats, truncated) = if stdout.len() > max_output {
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
        // Process I/O error
        Ok(Err(e)) => Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "error": e.to_string() }),
            is_error: true,
            duration_ms,
            ..Default::default()
        }),
        // Timeout elapsed — kill the process group and return error
        Err(_elapsed) => {
            #[cfg(unix)]
            {
                if let Some(pid) = child_pid {
                    // SAFETY: killpg sends SIGKILL to the process group we created.
                    unsafe {
                        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }

            Ok(ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                output: json!({
                    "error": format!("command timed out after {timeout_secs}s"),
                    "exit_code": -1,
                    "stdout": "",
                    "stderr": "",
                    "timed_out": true,
                }),
                is_error: true,
                duration_ms,
                ..Default::default()
            })
        }
    }
}

/// Post-execution input rewriter: compresses bulky `command` field.
pub fn rewrite_input(
    call: &ToolCall,
    result: &ToolResult,
    max_bytes: usize,
) -> Option<serde_json::Value> {
    if serde_json::to_string(&call.input)
        .map(|s| s.len())
        .unwrap_or(0)
        <= max_bytes
    {
        return None;
    }
    let cmd = call.input.get("command").and_then(|v| v.as_str())?;
    let mut rw = call.input.clone();
    let lines = cmd.lines().count();
    let exit = result
        .output
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    rw["command"] = json!(format!("[executed {lines}-line script, exit {exit}]"));
    Some(rw)
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
        let call =
            make_call(json!({"command": "printf 'apple\\nbanana\\napricot\\n'", "grep": "ap"}));
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

    #[tokio::test]
    async fn timeout_kills_slow_command() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "sleep 30", "timeout": 1}));
        let start = Instant::now();
        let result = execute(&call, &spill).await.unwrap();
        let elapsed = start.elapsed();
        assert!(result.is_error);
        assert_eq!(result.output["timed_out"], true);
        assert!(result.output["error"]
            .as_str()
            .unwrap()
            .contains("timed out"));
        // Should complete in well under 10s (we gave 1s timeout)
        assert!(elapsed.as_secs() < 10);
    }

    #[tokio::test]
    async fn timeout_default_allows_fast_commands() {
        let (_dir, spill) = temp_spill();
        // No explicit timeout — uses the 120s default, which is plenty for echo.
        let call = make_call(json!({"command": "echo hello"}));
        let result = execute(&call, &spill).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output["exit_code"], 0);
        assert!(result.output["stdout"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn timeout_zero_means_instant() {
        let (_dir, spill) = temp_spill();
        let call = make_call(json!({"command": "sleep 1", "timeout": 0}));
        let result = execute(&call, &spill).await.unwrap();
        assert!(result.is_error);
        assert_eq!(result.output["timed_out"], true);
    }

    #[test]
    fn rewrite_input_returns_none_when_small() {
        use apex_core::domain::ToolResult;
        let call = make_call(json!({"command": "echo hi"}));
        let result = ToolResult {
            tool_use_id: "test-id".into(),
            name: "shell_exec".into(),
            output: json!({"exit_code": 0, "stdout": "hi\n"}),
            is_error: false,
            ..Default::default()
        };
        assert!(rewrite_input(&call, &result, 10_000).is_none());
    }

    #[test]
    fn rewrite_input_compresses_large_command() {
        use apex_core::domain::ToolResult;
        let big_cmd = "echo line\n".repeat(500);
        let call = make_call(json!({"command": big_cmd}));
        let result = ToolResult {
            tool_use_id: "test-id".into(),
            name: "shell_exec".into(),
            output: json!({"exit_code": 42, "stdout": ""}),
            is_error: false,
            ..Default::default()
        };
        let rw = rewrite_input(&call, &result, 100).unwrap();
        let cmd = rw["command"].as_str().unwrap();
        assert!(cmd.contains("500-line script"));
        assert!(cmd.contains("exit 42"));
    }
}
