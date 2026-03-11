use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use apex_core::domain::{
    slugify, Fact, FactId, Skill, SkillId, SkillStatus, SubtaskEntry, SubtaskStatus, ToolCall,
    ToolDef, ToolResult, ToolSchema,
};
use apex_core::error::ToolError;
use apex_core::ports::{MemoryStore, SkillStore, ToolRegistry, WorkingMemory};

/// Unified memory tool registry providing both working memory (scratchpad) and
/// long-term memory (facts, skills, strategies) tools.
///
/// 6 tools total:
/// - working_memory_read, working_memory_update (working memory / scratchpad)
/// - memory_store_fact, memory_query_facts (long-term facts)
/// - memory_store_skill, memory_query_skill (long-term skills)
pub struct MemoryToolRegistry {
    memory: Arc<dyn WorkingMemory>,
    store: Arc<dyn MemoryStore>,
    skill_store: Arc<dyn SkillStore>,
}

impl MemoryToolRegistry {
    pub fn new(
        memory: Arc<dyn WorkingMemory>,
        store: Arc<dyn MemoryStore>,
        skill_store: Arc<dyn SkillStore>,
    ) -> Self {
        Self {
            memory,
            store,
            skill_store,
        }
    }
}

#[async_trait]
impl ToolRegistry for MemoryToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![
            // ── Working Memory ──────────────────────────────────────
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
            // ── Long-Term Memory: Facts ─────────────────────────────
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
            // ── Long-Term Memory: Skills ────────────────────────────
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
                            "name": {
                                "type": "string",
                                "description": "Slug name for the skill (e.g. 'install-package'). Derived from task_pattern if omitted."
                            },
                            "description": {
                                "type": "string",
                                "description": "Human-readable one-liner describing the skill. Defaults to task_pattern if omitted."
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
            "memory_store_skill" => self.exec_store_skill(call).await,
            "memory_query_skill" => self.exec_query_skill(call).await,
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

    async fn exec_store_skill(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
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

        let name = call
            .input
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| slugify(task_pattern));
        let description = call
            .input
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| task_pattern.to_string());

        let skill = Skill {
            id: SkillId(String::new()),
            name,
            description,
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
            status: SkillStatus::Active,
        };

        let id = self
            .skill_store
            .store_skill(skill)
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

    async fn exec_query_skill(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let task_pattern = call.input["task_pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing task_pattern".into()))?;

        let skill = self
            .skill_store
            .find_skill(task_pattern)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let output = match skill {
            Some(s) => json!({
                "found": true,
                "id": s.id.0,
                "name": s.name,
                "description": s.description,
                "task_pattern": s.task_pattern,
                "approach": s.approach,
                "tools_used": s.tools_used,
                "criteria_template": s.criteria_template,
                "fitness": format!("{:.2}", s.fitness),
                "success_count": s.success_count,
                "failure_count": s.failure_count,
            }),
            None => json!({ "found": false }),
        };

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output,
            is_error: false,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::{CalibrationData, Fact, FactId, Scratchpad, Skill, SkillId};
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

    struct MockSkillStore {
        skills: Mutex<Vec<Skill>>,
    }

    impl MockSkillStore {
        fn new() -> Self {
            Self {
                skills: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SkillStore for MockSkillStore {
        async fn store_skill(&self, skill: Skill) -> Result<SkillId, MemoryError> {
            let id = if skill.id.0.is_empty() {
                SkillId(format!("skill-{}", self.skills.lock().await.len()))
            } else {
                skill.id.clone()
            };
            self.skills.lock().await.push(Skill {
                id: id.clone(),
                ..skill
            });
            Ok(id)
        }
        async fn find_skill(&self, task_pattern: &str) -> Result<Option<Skill>, MemoryError> {
            let skills = self.skills.lock().await;
            Ok(skills
                .iter()
                .find(|s| s.task_pattern.contains(task_pattern))
                .cloned())
        }
        async fn list_skills(&self, limit: usize) -> Result<Vec<Skill>, MemoryError> {
            Ok(self
                .skills
                .lock()
                .await
                .iter()
                .take(limit)
                .cloned()
                .collect())
        }
        async fn update_skill_fitness(
            &self,
            _id: &SkillId,
            _success: bool,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    fn setup() -> MemoryToolRegistry {
        let wm: Arc<dyn WorkingMemory> = Arc::new(MockWorkingMemory::new());
        let lt: Arc<dyn MemoryStore> = Arc::new(MockMemoryStore::new());
        let skills: Arc<dyn SkillStore> = Arc::new(MockSkillStore::new());
        MemoryToolRegistry::new(wm, lt, skills)
    }

    #[test]
    fn definitions_returns_6_tools() {
        let reg = setup();
        let defs = reg.definitions();
        assert_eq!(defs.len(), 6);
        let names: Vec<&str> = defs.iter().map(|d| d.schema.name.as_str()).collect();
        assert!(names.contains(&"working_memory_read"));
        assert!(names.contains(&"working_memory_update"));
        assert!(names.contains(&"memory_store_fact"));
        assert!(names.contains(&"memory_query_facts"));
        assert!(names.contains(&"memory_store_skill"));
        assert!(names.contains(&"memory_query_skill"));
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
