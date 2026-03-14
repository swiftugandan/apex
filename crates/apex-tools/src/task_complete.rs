use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use apex_core::TASK_COMPLETE_TOOL;
use serde_json::json;

/// Tool definition for the explicit completion signal.
pub fn definition() -> ToolDef {
    ToolDef::eager(ToolSchema {
        name: TASK_COMPLETE_TOOL.into(),
        description: "Signal that you have completed the current task. \
            The result field should summarize what was accomplished."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "result": {
                    "type": "string",
                    "description": "Concise summary of the task result."
                }
            },
            "required": ["result"]
        }),
    })
}

/// Safety net — should never be called normally (loop intercepts first).
pub async fn execute(call: &ToolCall) -> Result<ToolResult, ToolError> {
    let result_text = call.input["result"].as_str().unwrap_or("Task completed.");
    Ok(ToolResult {
        tool_use_id: call.id.clone(),
        name: call.name.clone(),
        output: json!({ "status": "completed", "result": result_text }),
        is_error: false,
        ..Default::default()
    })
}
