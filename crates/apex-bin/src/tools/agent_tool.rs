use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use apex_core::context::TokenEstimator;
use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use apex_core::ports::{LlmProvider, MemoryStore, ToolRegistry};
use async_trait::async_trait;

use super::BuiltinToolRegistry;

/// Tool registry for the generic `agent` tool.
///
/// Spawns a sub-agent with a given system prompt, task, and filtered tool access.
/// The sub-agent runs `run_agentic_loop` and returns its final text response.
pub struct AgentToolRegistry {
    llm: Arc<dyn LlmProvider>,
    long_term: Arc<dyn MemoryStore>,
    estimator: Arc<Mutex<TokenEstimator>>,
    max_tool_result_bytes: usize,
    /// Own builtin tools (shell_exec, file_read, file_write) for sub-agents.
    builtin: BuiltinToolRegistry,
}

impl AgentToolRegistry {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        long_term: Arc<dyn MemoryStore>,
        estimator: Arc<Mutex<TokenEstimator>>,
        max_tool_result_bytes: usize,
    ) -> Self {
        Self {
            llm,
            long_term,
            estimator,
            max_tool_result_bytes,
            builtin: BuiltinToolRegistry::default(),
        }
    }
}

#[async_trait]
impl ToolRegistry for AgentToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            schema: ToolSchema {
                name: "agent".to_string(),
                description: "Spawn a sub-agent with a specific system prompt and tool access. \
                    The sub-agent runs independently and returns its final response. \
                    Use this for tasks that benefit from a separate perspective, like \
                    verifying your work or analyzing output with different expertise."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "system_prompt": {
                            "type": "string",
                            "description": "The sub-agent's persona and instructions"
                        },
                        "task": {
                            "type": "string",
                            "description": "What the sub-agent should do"
                        },
                        "tools": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Tool names the sub-agent can use (e.g. [\"shell_exec\", \"file_read\"]). Only builtin tools (shell_exec, file_read, file_write) are available."
                        }
                    },
                    "required": ["system_prompt", "task", "tools"]
                }),
            },
        }]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if call.name != "agent" {
            return Err(ToolError::UnknownTool(call.name.clone()));
        }

        let system_prompt = call.input.get("system_prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'system_prompt' field".into()))?
            .to_string();

        let task = call.input.get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'task' field".into()))?
            .to_string();

        let tool_names: Vec<String> = call.input.get("tools")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidInput("missing 'tools' field".into()))?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();

        // Build filtered tool registry from builtin tools, excluding "agent" itself
        let mut allowed: HashSet<String> = tool_names.into_iter().collect();
        allowed.remove("agent"); // prevent recursion

        let filtered = FilteredToolRegistry::new(&self.builtin, allowed);

        let available: Vec<String> = filtered.definitions().iter()
            .map(|d| d.schema.name.clone())
            .collect();

        eprintln!("  [sub-agent] starting with tools: {:?}", available);

        let messages = vec![apex_core::domain::ChatMessage::user_text(&task)];

        let (_turns, final_text, _messages) = crate::agent::run_agentic_loop(
            messages,
            &system_prompt,
            self.llm.as_ref(),
            &filtered,
            self.long_term.as_ref(),
            &self.estimator,
            self.max_tool_result_bytes,
            None, // sub-agents don't persist logs
            None,
        )
        .await;

        let output_text = final_text.unwrap_or_else(|| "(sub-agent produced no response)".to_string());

        eprintln!("  [sub-agent] finished");

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: serde_json::json!({ "response": output_text }),
            is_error: false,
            ..Default::default()
        })
    }
}

/// A filtered view of a ToolRegistry that only exposes allowed tool names.
struct FilteredToolRegistry<'a> {
    inner: &'a dyn ToolRegistry,
    allowed: HashSet<String>,
}

impl<'a> FilteredToolRegistry<'a> {
    fn new(inner: &'a dyn ToolRegistry, allowed: HashSet<String>) -> Self {
        Self { inner, allowed }
    }
}

#[async_trait]
impl ToolRegistry for FilteredToolRegistry<'_> {
    fn definitions(&self) -> Vec<ToolDef> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|d| self.allowed.contains(&d.schema.name))
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if !self.allowed.contains(&call.name) {
            return Err(ToolError::UnknownTool(call.name.clone()));
        }
        self.inner.execute(call).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_tool_definition() {
        // We can't easily construct AgentToolRegistry without real deps,
        // but we can test FilteredToolRegistry
        let builtin = BuiltinToolRegistry::default();
        let allowed: HashSet<String> = ["shell_exec", "file_read"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let filtered = FilteredToolRegistry::new(&builtin, allowed);
        let defs = filtered.definitions();
        assert_eq!(defs.len(), 2);
        let names: Vec<&str> = defs.iter().map(|d| d.schema.name.as_str()).collect();
        assert!(names.contains(&"shell_exec"));
        assert!(names.contains(&"file_read"));
        assert!(!names.contains(&"file_write"));
    }

    #[test]
    fn filtered_registry_excludes_unlisted() {
        let builtin = BuiltinToolRegistry::default();
        let allowed: HashSet<String> = ["file_read"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let filtered = FilteredToolRegistry::new(&builtin, allowed);
        assert_eq!(filtered.definitions().len(), 1);
        assert_eq!(filtered.definitions()[0].schema.name, "file_read");
    }
}
