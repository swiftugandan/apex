use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use apex_core::domain::{
    Fact, FactId, Skill, SkillId, Strategy, StrategyId, SubtaskEntry, SubtaskStatus, ToolCall,
    ToolDef, ToolResult, ToolSchema,
};
use apex_core::error::ToolError;
use apex_core::ports::{MemoryStore, ToolRegistry, WorkingMemory};

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

// ── Long-Term Memory Tool Registry ──────────────────────────────────

pub struct LongTermMemoryToolRegistry {
    store: Arc<dyn MemoryStore>,
}

impl LongTermMemoryToolRegistry {
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ToolRegistry for LongTermMemoryToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                schema: ToolSchema {
                    name: "memory_store_fact".into(),
                    description: "Store a discovered fact in long-term memory for future reference. Facts persist across jobs and are retrieved when relevant to new tasks.".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "The fact content to store"
                            },
                            "tags": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Tags for categorizing this fact"
                            },
                            "source_job": {
                                "type": "string",
                                "description": "The job ID that discovered this fact"
                            }
                        },
                        "required": ["content"]
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "memory_query_facts".into(),
                    description: "Query long-term memory for facts matching a search query. Returns facts with decayed confidence scores.".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Search query to match against fact content and tags"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of facts to return (default: 5)"
                            }
                        },
                        "required": ["query"]
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "memory_store_skill".into(),
                    description: "Store or update a skill (successful approach for a task pattern) in long-term memory. If a skill with the same task_pattern exists, it is updated.".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "task_pattern": {
                                "type": "string",
                                "description": "Pattern describing what kind of task this skill applies to"
                            },
                            "approach": {
                                "type": "string",
                                "description": "Description of the approach/strategy used"
                            },
                            "tools_used": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "List of tools used in this approach"
                            },
                            "criteria_template": {
                                "type": "string",
                                "description": "Optional acceptance criteria template for this task type"
                            },
                            "notes": {
                                "type": "string",
                                "description": "Additional notes about this skill"
                            }
                        },
                        "required": ["task_pattern", "approach"]
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "memory_query_skill".into(),
                    description: "Find the best matching skill for a task pattern. Returns the highest-fitness non-retired skill.".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "task_pattern": {
                                "type": "string",
                                "description": "Task pattern to search for"
                            }
                        },
                        "required": ["task_pattern"]
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "memory_store_strategy".into(),
                    description: "Store or update a decomposition strategy for a goal pattern.".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "goal_pattern": {
                                "type": "string",
                                "description": "Pattern describing the goal this strategy applies to"
                            },
                            "decomposition": {
                                "type": "string",
                                "description": "Description of how the goal was decomposed into subtasks"
                            },
                            "avg_subtasks": {
                                "type": "number",
                                "description": "Average number of subtasks in this decomposition"
                            },
                            "notes": {
                                "type": "string",
                                "description": "Additional notes"
                            }
                        },
                        "required": ["goal_pattern", "decomposition"]
                    }),
                },
            },
        ]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let result = match call.name.as_str() {
            "memory_store_fact" => self.exec_store_fact(call).await?,
            "memory_query_facts" => self.exec_query_facts(call).await?,
            "memory_store_skill" => self.exec_store_skill(call).await?,
            "memory_query_skill" => self.exec_query_skill(call).await?,
            "memory_store_strategy" => self.exec_store_strategy(call).await?,
            _ => return Err(ToolError::UnknownTool(call.name.clone())),
        };

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: result,
            is_error: false,
        })
    }
}

impl LongTermMemoryToolRegistry {
    async fn exec_store_fact(&self, call: &ToolCall) -> Result<Value, ToolError> {
        let content = call.input["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing content".into()))?;
        let tags: Vec<String> = call
            .input
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let source_job = call
            .input
            .get("source_job")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let fact = Fact {
            id: FactId(String::new()),
            content: content.to_string(),
            source_job,
            confidence: 1.0,
            created_at: String::new(),
            last_verified: String::new(),
            tags,
        };

        let id = self
            .store
            .store_fact(fact)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(json!({ "id": id.0, "status": "stored" }))
    }

    async fn exec_query_facts(&self, call: &ToolCall) -> Result<Value, ToolError> {
        let query = call.input["query"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing query".into()))?;
        let limit = call
            .input
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        let facts = self
            .store
            .query_facts(query, limit)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let results: Vec<Value> = facts
            .iter()
            .map(|f| {
                json!({
                    "id": f.id.0,
                    "content": f.content,
                    "confidence": format!("{:.2}", f.confidence),
                    "tags": f.tags,
                    "source_job": f.source_job,
                })
            })
            .collect();

        Ok(json!({ "count": results.len(), "facts": results }))
    }

    async fn exec_store_skill(&self, call: &ToolCall) -> Result<Value, ToolError> {
        let task_pattern = call.input["task_pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing task_pattern".into()))?;
        let approach = call.input["approach"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing approach".into()))?;
        let tools_used: Vec<String> = call
            .input
            .get("tools_used")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let criteria_template = call
            .input
            .get("criteria_template")
            .and_then(|v| v.as_str())
            .map(String::from);
        let notes = call
            .input
            .get("notes")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let skill = Skill {
            id: SkillId(String::new()),
            task_pattern: task_pattern.to_string(),
            approach: approach.to_string(),
            tools_used,
            criteria_template,
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            min_samples: 3,
            last_used: String::new(),
            notes,
        };

        let id = self
            .store
            .store_skill(skill)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(json!({ "id": id.0, "status": "stored" }))
    }

    async fn exec_query_skill(&self, call: &ToolCall) -> Result<Value, ToolError> {
        let task_pattern = call.input["task_pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing task_pattern".into()))?;

        let skill = self
            .store
            .find_skill(task_pattern)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        match skill {
            Some(s) => Ok(json!({
                "found": true,
                "id": s.id.0,
                "task_pattern": s.task_pattern,
                "approach": s.approach,
                "tools_used": s.tools_used,
                "criteria_template": s.criteria_template,
                "fitness": format!("{:.2}", s.fitness),
                "success_count": s.success_count,
                "failure_count": s.failure_count,
            })),
            None => Ok(json!({ "found": false })),
        }
    }

    async fn exec_store_strategy(&self, call: &ToolCall) -> Result<Value, ToolError> {
        let goal_pattern = call.input["goal_pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing goal_pattern".into()))?;
        let decomposition = call.input["decomposition"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing decomposition".into()))?;
        let avg_subtasks = call
            .input
            .get("avg_subtasks")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let notes = call
            .input
            .get("notes")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let strategy = Strategy {
            id: StrategyId(String::new()),
            goal_pattern: goal_pattern.to_string(),
            decomposition: decomposition.to_string(),
            avg_subtasks,
            avg_duration_secs: 0.0,
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            notes,
        };

        let id = self
            .store
            .store_strategy(strategy)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(json!({ "id": id.0, "status": "stored" }))
    }
}
