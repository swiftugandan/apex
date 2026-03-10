use std::sync::Arc;

use apex_core::config::{MemoryMode, RoleProfile};
use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use apex_core::ports::ToolRegistry;
use async_trait::async_trait;

use std::path::PathBuf;

/// Result returned after a sub-agent completes.
pub struct SubAgentResult {
    pub done_bodies: Vec<String>,
    pub failed_bodies: Vec<String>,
}

/// Trait for spawning sub-agent processes. Decouples the delegate tool from
/// concrete queue/memory/LLM provisioning.
#[async_trait]
pub trait SubAgentSpawner: Send + Sync {
    async fn spawn(
        &self,
        task: &str,
        role: &RoleProfile,
        persona: &str,
    ) -> Result<SubAgentResult, ToolError>;
}

/// Tool registry for the `delegate` tool.
///
/// Spawns a full sub-agent subprocess with its own queue, working memory,
/// and scratchpad — just like the main worker process.
pub struct DelegateToolRegistry {
    roles: Arc<[RoleProfile]>,
    prompts_dir: PathBuf,
    spawner: Arc<dyn SubAgentSpawner>,
    remaining_depth: u32,
}

impl DelegateToolRegistry {
    pub fn new(
        roles: Arc<[RoleProfile]>,
        prompts_dir: PathBuf,
        spawner: Arc<dyn SubAgentSpawner>,
        remaining_depth: u32,
    ) -> Self {
        Self {
            roles,
            prompts_dir,
            spawner,
            remaining_depth,
        }
    }

    /// Resolve a role profile from a named role or inline ad-hoc parameters.
    async fn resolve_role(&self, call: &ToolCall) -> Result<(RoleProfile, String), ToolError> {
        let role_name = call.input.get("role").and_then(|v| v.as_str());
        let system_prompt_val = call.input.get("system_prompt").and_then(|v| v.as_str());

        if role_name.is_some() && system_prompt_val.is_some() {
            return Err(ToolError::InvalidInput(
                "provide either 'role' or 'system_prompt', not both".into(),
            ));
        }

        if let Some(role_name) = role_name {
            let role = self
                .roles
                .iter()
                .find(|r| r.name == role_name)
                .ok_or_else(|| {
                    let available: Vec<&str> = self.roles.iter().map(|r| r.name.as_str()).collect();
                    ToolError::InvalidInput(format!(
                        "unknown role '{}'. Available roles: {:?}",
                        role_name, available
                    ))
                })?
                .clone();

            // Load persona from file or use default
            let persona_file = role.persona.as_deref().unwrap_or("agent.md");
            let path = self.prompts_dir.join(persona_file);
            let persona = tokio::fs::read_to_string(&path).await.map_err(|e| {
                ToolError::Execution(format!(
                    "failed to read persona file '{}': {e}",
                    path.display()
                ))
            })?;

            Ok((role, persona))
        } else if let Some(system_prompt_str) = system_prompt_val {
            let system_prompt = system_prompt_str.to_string();

            let tools: Vec<String> = call
                .input
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let max_depth = call
                .input
                .get("max_depth")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32)
                .unwrap_or(1);

            let can_delegate = call
                .input
                .get("can_delegate")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let role = RoleProfile {
                name: "ad-hoc".to_string(),
                persona: None,
                model: None,
                tools,
                max_depth,
                max_retries: 3,
                max_concurrent: 1,
                memory: MemoryMode::Shared,
                can_delegate,
            };

            Ok((role, system_prompt))
        } else {
            Err(ToolError::InvalidInput(
                "must provide either 'role' (named role) or 'system_prompt' (ad-hoc role)".into(),
            ))
        }
    }
}

#[async_trait]
impl ToolRegistry for DelegateToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            schema: ToolSchema {
                name: "delegate".to_string(),
                description: "Delegate a task to a sub-agent with a specific role. \
                    Use a named role (from config) or define an ad-hoc role inline. \
                    The sub-agent runs as a full subprocess with its own queue, working memory, \
                    and scratchpad. Delegation is blocking — this tool returns when the sub-agent completes."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "role": {
                            "type": "string",
                            "description": "Name of a pre-defined role from config (e.g. 'coder', 'reviewer'). Mutually exclusive with 'system_prompt'."
                        },
                        "task": {
                            "type": "string",
                            "description": "What the sub-agent should accomplish"
                        },
                        "system_prompt": {
                            "type": "string",
                            "description": "Ad-hoc system prompt for the sub-agent. Mutually exclusive with 'role'."
                        },
                        "tools": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Tool names the ad-hoc sub-agent can use (e.g. [\"shell_exec\", \"file_read\"]). Only used with 'system_prompt', ignored for named roles."
                        },
                        "max_depth": {
                            "type": "integer",
                            "description": "Max recursion depth for ad-hoc sub-agent (default: 1). Only used with 'system_prompt'."
                        },
                        "can_delegate": {
                            "type": "boolean",
                            "description": "Whether the ad-hoc sub-agent can further delegate (default: false). Only used with 'system_prompt'."
                        }
                    },
                    "required": ["task"]
                }),
            },
        }]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if call.name != "delegate" {
            return Err(ToolError::UnknownTool(call.name.clone()));
        }

        // Check depth limit
        if self.remaining_depth == 0 {
            return Err(ToolError::Execution(
                "delegation depth limit reached — cannot spawn further sub-agents".into(),
            ));
        }

        let task = call
            .input
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("missing 'task' field".into()))?
            .to_string();

        let (role, persona) = self.resolve_role(call).await?;

        eprintln!(
            "  [delegate:{}] spawning subprocess",
            role.name,
        );

        let result = self.spawner.spawn(&task, &role, &persona).await?;

        eprintln!("  [delegate:{}] finished ({} done, {} failed)",
            role.name, result.done_bodies.len(), result.failed_bodies.len());

        // Convert SubAgentResult to ToolResult
        if !result.failed_bodies.is_empty() {
            let failure_summary = result.failed_bodies.join("\n---\n");
            return Err(ToolError::Execution(format!(
                "sub-agent '{}' failed:\n{}", role.name, failure_summary
            )));
        }

        let response = if result.done_bodies.is_empty() {
            "(sub-agent produced no results)".to_string()
        } else {
            result.done_bodies.join("\n---\n")
        };

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: serde_json::json!({
                "role": role.name,
                "response": response,
            }),
            is_error: false,
            ..Default::default()
        })
    }
}
