use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use apex_core::domain::{SubtaskEntry, SubtaskStatus, ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use apex_core::ports::{ToolRegistry, WorkingMemory};

pub struct MemoryToolRegistry {
    memory: Arc<dyn WorkingMemory>,
}

impl MemoryToolRegistry {
    pub fn new(memory: Arc<dyn WorkingMemory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl ToolRegistry for MemoryToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                schema: ToolSchema {
                    name: "working_memory_read".into(),
                    description: "Read the working memory scratchpad for a job. Returns the current decomposition state, notes, and status.".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "job_id": {
                                "type": "string",
                                "description": "The job ID to read scratchpad for"
                            }
                        },
                        "required": ["job_id"]
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "working_memory_update".into(),
                    description: "Update the working memory scratchpad for a job. Apply structured changes to goal, subtasks, notes, or status.".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "job_id": {
                                "type": "string",
                                "description": "The job ID to update scratchpad for"
                            },
                            "goal": {
                                "type": "string",
                                "description": "Optional new goal to set"
                            },
                            "add_subtask": {
                                "type": "object",
                                "description": "Add a new subtask",
                                "properties": {
                                    "description": { "type": "string" },
                                    "task_id": { "type": "string" },
                                    "depends_on": { "type": "string" }
                                },
                                "required": ["description"]
                            },
                            "update_subtask": {
                                "type": "object",
                                "description": "Update an existing subtask's status",
                                "properties": {
                                    "index": { "type": "integer" },
                                    "status": { "type": "string", "enum": ["done", "active", "pending"] }
                                },
                                "required": ["index", "status"]
                            },
                            "add_note": {
                                "type": "string",
                                "description": "Add a note to the scratchpad"
                            },
                            "status_summary": {
                                "type": "string",
                                "description": "Optional new status summary"
                            }
                        },
                        "required": ["job_id"]
                    }),
                },
            },
        ]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        match call.name.as_str() {
            "working_memory_read" => self.exec_read(call).await,
            "working_memory_update" => self.exec_update(call).await,
            _ => Err(ToolError::UnknownTool(call.name.clone())),
        }
    }
}

impl MemoryToolRegistry {
    async fn exec_read(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let job_id = call.input["job_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing job_id".into()))?;

        let pad = self
            .memory
            .load_or_create(job_id)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "content": pad.to_markdown() }),
            is_error: false,
        })
    }

    async fn exec_update(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let job_id = call.input["job_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing job_id".into()))?;

        let mut pad = self
            .memory
            .load_or_create(job_id)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        // Apply goal update
        if let Some(goal) = call.input.get("goal").and_then(Value::as_str) {
            pad.goal = goal.to_string();
        }

        // Apply status_summary update
        if let Some(summary) = call.input.get("status_summary").and_then(Value::as_str) {
            pad.status_summary = summary.to_string();
        }

        // Add note
        if let Some(note) = call.input.get("add_note").and_then(Value::as_str) {
            pad.notes.push(note.to_string());
        }

        // Add subtask
        if let Some(st) = call.input.get("add_subtask") {
            let desc = st["description"]
                .as_str()
                .ok_or_else(|| ToolError::InvalidInput("add_subtask.description required".into()))?;
            let next_index = pad.subtasks.last().map_or(1, |s| s.index + 1);
            pad.subtasks.push(SubtaskEntry {
                index: next_index,
                description: desc.to_string(),
                status: SubtaskStatus::Pending,
                task_id: st.get("task_id").and_then(Value::as_str).map(String::from),
                depends_on: st.get("depends_on").and_then(Value::as_str).map(String::from),
            });
        }

        // Update subtask status
        if let Some(upd) = call.input.get("update_subtask") {
            let index = upd["index"]
                .as_u64()
                .ok_or_else(|| ToolError::InvalidInput("update_subtask.index required".into()))?
                as u32;
            let status_str = upd["status"]
                .as_str()
                .ok_or_else(|| ToolError::InvalidInput("update_subtask.status required".into()))?;
            let status = match status_str {
                "done" => SubtaskStatus::Done,
                "active" => SubtaskStatus::Active,
                "pending" => SubtaskStatus::Pending,
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "invalid status: {other}"
                    )))
                }
            };
            if let Some(entry) = pad.subtasks.iter_mut().find(|s| s.index == index) {
                entry.status = status;
            } else {
                return Err(ToolError::InvalidInput(format!(
                    "subtask index {index} not found"
                )));
            }
        }

        self.memory
            .save(&pad)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "content": pad.to_markdown() }),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_returns_2_tools() {
        let store = Arc::new(crate::FsScratchpadStore::new("/tmp/test".into()));
        let reg = MemoryToolRegistry::new(store);
        let defs = reg.definitions();
        assert_eq!(defs.len(), 2);
        let names: Vec<&str> = defs.iter().map(|d| d.schema.name.as_str()).collect();
        assert!(names.contains(&"working_memory_read"));
        assert!(names.contains(&"working_memory_update"));
    }

    #[tokio::test]
    async fn read_returns_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::FsScratchpadStore::new(dir.path().to_path_buf()));
        let reg = MemoryToolRegistry::new(store);

        let call = ToolCall {
            id: "t1".into(),
            name: "working_memory_read".into(),
            input: json!({ "job_id": "job-10" }),
        };
        let result = reg.execute(&call).await.unwrap();
        assert!(!result.is_error);
        let content = result.output["content"].as_str().unwrap();
        assert!(content.contains("# Working Memory: job-10"));
    }

    #[tokio::test]
    async fn update_applies_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::FsScratchpadStore::new(dir.path().to_path_buf()));
        let reg = MemoryToolRegistry::new(store);

        // Add a subtask
        let call = ToolCall {
            id: "t1".into(),
            name: "working_memory_update".into(),
            input: json!({
                "job_id": "job-20",
                "goal": "Build the thing",
                "add_subtask": { "description": "Step 1", "task_id": "task-001" },
                "add_note": "Starting work",
                "status_summary": "In progress"
            }),
        };
        let result = reg.execute(&call).await.unwrap();
        assert!(!result.is_error);
        let content = result.output["content"].as_str().unwrap();
        assert!(content.contains("Build the thing"));
        assert!(content.contains("[pending] Step 1"));
        assert!(content.contains("task-001"));
        assert!(content.contains("Starting work"));
        assert!(content.contains("In progress"));

        // Update the subtask to done
        let call2 = ToolCall {
            id: "t2".into(),
            name: "working_memory_update".into(),
            input: json!({
                "job_id": "job-20",
                "update_subtask": { "index": 1, "status": "done" }
            }),
        };
        let result2 = reg.execute(&call2).await.unwrap();
        let content2 = result2.output["content"].as_str().unwrap();
        assert!(content2.contains("[done] Step 1"));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::FsScratchpadStore::new(dir.path().to_path_buf()));
        let reg = MemoryToolRegistry::new(store);

        let call = ToolCall {
            id: "t1".into(),
            name: "nonexistent".into(),
            input: json!({}),
        };
        let err = reg.execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(_)));
    }
}
