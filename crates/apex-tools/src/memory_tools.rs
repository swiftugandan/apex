use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use apex_core::domain::{
    Fact, FactId, SubtaskEntry, SubtaskStatus, ToolCall, ToolDef, ToolResult, ToolSchema,
};
use apex_core::error::ToolError;
use apex_core::ports::{MemoryStore, ToolRegistry, WorkingMemory};

/// Memory tool registry providing working memory (scratchpad) and
/// long-term memory (facts) tools.
///
/// 4 tools total:
/// - working_memory_read, working_memory_update (working memory / scratchpad)
/// - memory_store_fact, memory_query_facts (long-term facts)
///
/// Skill tools (list_skills, use_skill, store_skill) are in SkillToolRegistry.
pub struct MemoryToolRegistry {
    memory: Arc<dyn WorkingMemory>,
    store: Arc<dyn MemoryStore>,
}

impl MemoryToolRegistry {
    pub fn new(memory: Arc<dyn WorkingMemory>, store: Arc<dyn MemoryStore>) -> Self {
        Self { memory, store }
    }
}

#[async_trait]
impl ToolRegistry for MemoryToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![
            // ── Working Memory ──────────────────────────────────────
            ToolDef::eager(ToolSchema {
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
            }),
            ToolDef::eager(ToolSchema {
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
            }),
            // ── Long-Term Memory: Facts ─────────────────────────────
            ToolDef::eager(ToolSchema {
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
            }),
            ToolDef::eager(ToolSchema {
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
            }),
        ]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        match call.name.as_str() {
            // Working memory
            "working_memory_read" => self.exec_wm_read(call).await,
            "working_memory_update" => self.exec_wm_update(call).await,
            // Long-term memory
            "memory_store_fact" => self.exec_store_fact(call).await,
            "memory_query_facts" => self.exec_query_facts(call).await,
            _ => Err(ToolError::UnknownTool(call.name.clone())),
        }
    }
}

// ── Working Memory implementations ──────────────────────────────────

impl MemoryToolRegistry {
    async fn exec_wm_read(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
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
            ..Default::default()
        })
    }

    async fn exec_wm_update(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let job_id = call.input["job_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing job_id".into()))?;

        let mut pad = self
            .memory
            .load_or_create(job_id)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        if let Some(goal) = call.input.get("goal").and_then(Value::as_str) {
            pad.goal = goal.to_string();
        }

        if let Some(summary) = call.input.get("status_summary").and_then(Value::as_str) {
            pad.status_summary = summary.to_string();
        }

        if let Some(note) = call.input.get("add_note").and_then(Value::as_str) {
            pad.notes.push(note.to_string());
        }

        if let Some(st) = call.input.get("add_subtask") {
            let desc = st["description"].as_str().ok_or_else(|| {
                ToolError::InvalidInput("add_subtask.description required".into())
            })?;
            let next_index = pad.subtasks.last().map_or(1, |s| s.index + 1);
            pad.subtasks.push(SubtaskEntry {
                index: next_index,
                description: desc.to_string(),
                status: SubtaskStatus::Pending,
                task_id: st.get("task_id").and_then(Value::as_str).map(String::from),
                depends_on: st
                    .get("depends_on")
                    .and_then(Value::as_str)
                    .map(String::from),
            });
        }

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
                other => return Err(ToolError::InvalidInput(format!("invalid status: {other}"))),
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
            ..Default::default()
        })
    }
}

// ── Long-Term Memory implementations ────────────────────────────────

impl MemoryToolRegistry {
    async fn exec_store_fact(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
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

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "id": id.0, "status": "stored" }),
            is_error: false,
            ..Default::default()
        })
    }

    async fn exec_query_facts(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
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

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "count": results.len(), "facts": results }),
            is_error: false,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::{CalibrationData, Fact, FactId, Scratchpad};
    use apex_core::error::MemoryError;
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    // ── Inline mocks (no infra dependency) ──────────────────────────

    struct MockWorkingMemory {
        pads: Mutex<HashMap<String, Scratchpad>>,
    }

    impl MockWorkingMemory {
        fn new() -> Self {
            Self {
                pads: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl WorkingMemory for MockWorkingMemory {
        async fn load_or_create(&self, job_id: &str) -> Result<Scratchpad, MemoryError> {
            let pads = self.pads.lock().await;
            Ok(pads
                .get(job_id)
                .cloned()
                .unwrap_or_else(|| Scratchpad::new(job_id, "")))
        }
        async fn save(&self, scratchpad: &Scratchpad) -> Result<(), MemoryError> {
            self.pads
                .lock()
                .await
                .insert(scratchpad.job_id.clone(), scratchpad.clone());
            Ok(())
        }
        async fn exists(&self, job_id: &str) -> Result<bool, MemoryError> {
            Ok(self.pads.lock().await.contains_key(job_id))
        }
        async fn delete(&self, job_id: &str) -> Result<(), MemoryError> {
            self.pads.lock().await.remove(job_id);
            Ok(())
        }
        async fn list_active(&self) -> Result<Vec<String>, MemoryError> {
            Ok(self.pads.lock().await.keys().cloned().collect())
        }
        async fn reap_stale(&self, _retention_days: u32) -> Result<Vec<String>, MemoryError> {
            Ok(Vec::new())
        }
    }

    struct MockMemoryStore {
        facts: Mutex<Vec<Fact>>,
    }

    impl MockMemoryStore {
        fn new() -> Self {
            Self {
                facts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl MemoryStore for MockMemoryStore {
        async fn store_fact(&self, fact: Fact) -> Result<FactId, MemoryError> {
            let id = if fact.id.0.is_empty() {
                FactId(format!("fact-{}", self.facts.lock().await.len()))
            } else {
                fact.id.clone()
            };
            self.facts.lock().await.push(Fact {
                id: id.clone(),
                ..fact
            });
            Ok(id)
        }
        async fn query_facts(&self, query: &str, limit: usize) -> Result<Vec<Fact>, MemoryError> {
            let facts = self.facts.lock().await;
            Ok(facts
                .iter()
                .filter(|f| f.content.contains(query))
                .take(limit)
                .cloned()
                .collect())
        }
        async fn verify_fact(&self, _id: &FactId) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn persist_calibration(&self, _data: &CalibrationData) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn load_calibration(&self) -> Result<CalibrationData, MemoryError> {
            Ok(CalibrationData::default())
        }
    }

    fn setup() -> MemoryToolRegistry {
        let wm: Arc<dyn WorkingMemory> = Arc::new(MockWorkingMemory::new());
        let lt: Arc<dyn MemoryStore> = Arc::new(MockMemoryStore::new());
        MemoryToolRegistry::new(wm, lt)
    }

    #[test]
    fn definitions_returns_4_tools() {
        let reg = setup();
        let defs = reg.definitions();
        assert_eq!(defs.len(), 4);
        let names: Vec<&str> = defs.iter().map(|d| d.schema.name.as_str()).collect();
        assert!(names.contains(&"working_memory_read"));
        assert!(names.contains(&"working_memory_update"));
        assert!(names.contains(&"memory_store_fact"));
        assert!(names.contains(&"memory_query_facts"));
    }

    #[tokio::test]
    async fn working_memory_read_returns_content() {
        let reg = setup();
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
    async fn working_memory_update_applies_changes() {
        let reg = setup();
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
        assert!(content.contains("Starting work"));
    }

    #[tokio::test]
    async fn store_and_query_fact() {
        let reg = setup();

        // Store
        let call = ToolCall {
            id: "t1".into(),
            name: "memory_store_fact".into(),
            input: json!({ "content": "Rust is fast", "tags": ["lang"] }),
        };
        let result = reg.execute(&call).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output["status"], "stored");

        // Query
        let call = ToolCall {
            id: "t2".into(),
            name: "memory_query_facts".into(),
            input: json!({ "query": "Rust" }),
        };
        let result = reg.execute(&call).await.unwrap();
        assert!(!result.is_error);
        assert!(result.output["count"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let reg = setup();
        let call = ToolCall {
            id: "t1".into(),
            name: "nonexistent".into(),
            input: json!({}),
        };
        let err = reg.execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(_)));
    }
}
