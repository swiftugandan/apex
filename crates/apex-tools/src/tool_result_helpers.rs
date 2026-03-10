use std::time::Instant;

use apex_core::domain::{ToolCall, ToolResult};
use apex_core::error::ToolError;
use serde_json::{json, Value};

pub fn ok_result(call: &ToolCall, output: Value) -> Result<ToolResult, ToolError> {
    Ok(ToolResult {
        tool_use_id: call.id.clone(),
        name: call.name.clone(),
        output,
        is_error: false,
        ..Default::default()
    })
}

pub fn err_result(call: &ToolCall, message: &str) -> Result<ToolResult, ToolError> {
    Ok(ToolResult {
        tool_use_id: call.id.clone(),
        name: call.name.clone(),
        output: json!({ "error": message }),
        is_error: true,
        ..Default::default()
    })
}

pub fn error_result(call: &ToolCall, msg: &str, start: Instant) -> ToolResult {
    ToolResult {
        tool_use_id: call.id.clone(),
        name: call.name.clone(),
        output: json!({ "error": msg }),
        is_error: true,
        duration_ms: start.elapsed().as_millis() as u64,
        ..Default::default()
    }
}
