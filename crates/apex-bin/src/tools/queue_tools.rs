use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use apex_core::context::MessageComposer;
use apex_core::domain::{
    MessageHeaders, MessageType, QueueMessage, ToolCall, ToolDef, ToolResult, ToolSchema,
};
use apex_core::error::ToolError;
use apex_core::ports::{MemoryStore, Queue, ToolRegistry};

pub struct QueueToolRegistry {
    queue: Arc<dyn Queue>,
    correlation_id: String,
    current_depth: u32,
    max_depth: u32,
    parent_goal: String,
    parent_body: String,
    store: Option<Arc<dyn MemoryStore>>,
    composer: MessageComposer,
}

impl QueueToolRegistry {
    pub fn new(
        queue: Arc<dyn Queue>,
        correlation_id: String,
        current_depth: u32,
        max_depth: u32,
        parent_goal: String,
        parent_body: String,
        store: Option<Arc<dyn MemoryStore>>,
    ) -> Self {
        Self {
            queue,
            correlation_id,
            current_depth,
            max_depth,
            parent_goal,
            parent_body,
            store,
            composer: MessageComposer::default(),
        }
    }

    #[allow(dead_code)]
    pub fn with_composer(mut self, composer: MessageComposer) -> Self {
        self.composer = composer;
        self
    }

    async fn handle_decompose_goal(&self, input: &Value) -> Result<Value, ToolError> {
        if self.current_depth >= self.max_depth {
            return Err(ToolError::Execution(format!(
                "Max decomposition depth ({}) reached. Handle this task directly instead of decomposing further.",
                self.max_depth
            )));
        }

        let subtasks = input
            .get("subtasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidInput("missing 'subtasks' array".to_string()))?;

        if subtasks.is_empty() {
            return Err(ToolError::InvalidInput(
                "subtasks array must not be empty".to_string(),
            ));
        }

        struct SubtaskInfo {
            description: String,
            acceptance_criteria: String,
            depends_on_indices: Vec<usize>,
        }

        let mut infos = Vec::new();
        for (idx, subtask) in subtasks.iter().enumerate() {
            let description = subtask
                .get("description")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput(format!("subtask[{idx}] missing 'description'"))
                })?
                .to_string();

            let acceptance_criteria = subtask
                .get("acceptance_criteria")
                .and_then(|v| v.as_str())
                .unwrap_or("(to be determined by agent)")
                .to_string();

            let depends_on_indices: Vec<usize> = subtask
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect()
                })
                .unwrap_or_default();

            for &dep_idx in &depends_on_indices {
                if dep_idx >= subtasks.len() {
                    return Err(ToolError::InvalidInput(format!(
                        "subtask[{idx}] depends_on index {dep_idx} is out of range"
                    )));
                }
                if dep_idx == idx {
                    return Err(ToolError::InvalidInput(format!(
                        "subtask[{idx}] cannot depend on itself"
                    )));
                }
            }

            infos.push(SubtaskInfo {
                description,
                acceptance_criteria,
                depends_on_indices,
            });
        }

        let mut subtask_ids: Vec<String> = Vec::new();

        for info in &infos {
            let title = info.description.lines().next().unwrap_or(&info.description);
            let title = if title.len() > 80 { &title[..80] } else { title };

            let (facts, skill) = if let Some(ref store) = self.store {
                let facts = store.query_facts(&info.description, 3).await.ok().unwrap_or_default();
                let skill = store.find_skill(&info.description).await.ok().flatten();
                (facts, skill)
            } else {
                (Vec::new(), None)
            };

            let body = if !facts.is_empty() || skill.is_some() {
                self.composer.compose_subtask_with_memory(
                    title,
                    &info.description,
                    &info.acceptance_criteria,
                    &self.parent_goal,
                    &self.parent_body,
                    &facts,
                    skill.as_ref(),
                )
            } else {
                self.composer.compose_subtask(
                    title,
                    &info.description,
                    &info.acceptance_criteria,
                    &self.parent_goal,
                    &self.parent_body,
                )
            };

            let depends_on: Vec<String> = info
                .depends_on_indices
                .iter()
                .map(|&idx| subtask_ids[idx].clone())
                .collect();

            let msg = QueueMessage {
                headers: MessageHeaders {
                    message_type: MessageType::Subtask,
                    correlation_id: self.correlation_id.clone(),
                    depth: self.current_depth + 1,
                    retry_count: 0,
                    depends_on,
                },
                body,
            };

            let id = self
                .queue
                .push(msg)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            subtask_ids.push(id);
        }

        let continuation_body = MessageComposer::compose_continuation(
            &self.correlation_id,
            &self.parent_goal,
            &subtask_ids,
        );

        let continuation_msg = QueueMessage {
            headers: MessageHeaders {
                message_type: MessageType::Continuation,
                correlation_id: self.correlation_id.clone(),
                depth: self.current_depth,
                retry_count: 0,
                depends_on: subtask_ids.clone(),
            },
            body: continuation_body,
        };

        let continuation_id = self
            .queue
            .push(continuation_msg)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(json!({
            "status": "decomposed",
            "subtask_ids": subtask_ids,
            "continuation_id": continuation_id,
            "message": format!("Created {} subtask(s) and 1 continuation message", subtask_ids.len())
        }))
    }

    async fn handle_queue_read_done(&self, input: &Value) -> Result<Value, ToolError> {
        let correlation_id = input
            .get("correlation_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.correlation_id);

        let done_ids = self
            .queue
            .list_done(correlation_id)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let mut results = Vec::new();
        for id in &done_ids {
            let body = self
                .queue
                .read_done_body(id)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            results.push(json!({
                "id": id,
                "body": body,
            }));
        }

        Ok(json!({
            "correlation_id": correlation_id,
            "count": results.len(),
            "results": results,
        }))
    }
}

#[async_trait]
impl ToolRegistry for QueueToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                schema: ToolSchema {
                    name: "decompose_goal".to_string(),
                    description: "Decompose a complex goal into subtasks that will be executed independently and in parallel where possible. Use this when a task has 2 or more independent steps. Each subtask becomes a separate queue message processed by an agent instance. Write deterministic acceptance_criteria using the structured format so the system can auto-verify completion.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "subtasks": {
                                "type": "array",
                                "description": "List of subtasks to create",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "description": {
                                            "type": "string",
                                            "description": "What this subtask should accomplish"
                                        },
                                        "acceptance_criteria": {
                                            "type": "string",
                                            "description": "Deterministic checks to auto-verify completion. Use the structured format: start with '### Deterministic' followed by criteria lines. Command checks: '- command: `<cmd>`\\n  expect: exit_code 0' or 'output_contains \"text\"' or 'output_matches \"regex\"' or 'not_contains \"text\"'. Shorthand checks: '- file_exists \"/path\"', '- file_contains \"/path\" \"text\"', '- http_status \"url\" 200'. Example: '### Deterministic\\n- command: `test -f /tmp/out.txt`\\n  expect: exit_code 0\\n- command: `cat /tmp/out.txt`\\n  expect: output_contains \"hello\"'"
                                        },
                                        "depends_on": {
                                            "type": "array",
                                            "description": "Indices (0-based) of subtasks this depends on",
                                            "items": { "type": "integer" }
                                        }
                                    },
                                    "required": ["description"]
                                }
                            }
                        },
                        "required": ["subtasks"]
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "queue_read_done".to_string(),
                    description: "Read completed subtask results from the queue. Use this in continuation messages to collect results from all completed subtasks before assembling the final deliverable.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "correlation_id": {
                                "type": "string",
                                "description": "The correlation ID to filter by. Defaults to the current job's correlation ID."
                            }
                        }
                    }),
                },
            },
        ]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let result = match call.name.as_str() {
            "decompose_goal" => self.handle_decompose_goal(&call.input).await?,
            "queue_read_done" => self.handle_queue_read_done(&call.input).await?,
            _ => return Err(ToolError::UnknownTool(call.name.clone())),
        };

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: result,
            is_error: false,
            ..Default::default()
        })
    }
}
