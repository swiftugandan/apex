use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use apex_core::config::{Invariants, RoleProfile};
use crate::constants::COMPACT_CONVERSATION_TOOL;
use apex_core::domain::{
    Scratchpad, SubtaskEntry, SubtaskStatus, ToolCall, ToolDef, ToolResult, ToolSchema,
};
use apex_core::error::ToolError;
use apex_core::ports::{MemoryStore, SkillStore, ToolRegistry, WorkingMemory};

use apex_tools::{
    BuiltinToolRegistry, ConfigToolRegistry, CustomToolRegistry, DelegateToolRegistry,
    MemoryToolRegistry, QueueToolRegistry, SubAgentSpawner,
};
use apex_tools::spill::SpillManager;

use crate::ProjectPaths;

// ── CompositeToolRegistry ──────────────────────────────────────────

/// Aggregates multiple tool registries with O(1) dispatch via HashMap.
pub struct CompositeToolRegistry {
    by_name: HashMap<String, usize>,
    registries: Vec<Box<dyn ToolRegistry>>,
    cached_defs: Arc<Vec<ToolDef>>,
}

impl CompositeToolRegistry {
    pub fn new(registries: Vec<Box<dyn ToolRegistry>>) -> Self {
        let mut by_name = HashMap::new();
        let mut cached_defs = Vec::new();
        for (idx, registry) in registries.iter().enumerate() {
            for def in registry.definitions() {
                by_name.insert(def.schema.name.clone(), idx);
                cached_defs.push(def);
            }
        }
        Self {
            by_name,
            registries,
            cached_defs: Arc::new(cached_defs),
        }
    }
}

#[async_trait]
impl ToolRegistry for CompositeToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        self.cached_defs.as_ref().clone()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if let Some(&idx) = self.by_name.get(&call.name) {
            self.registries[idx].execute(call).await
        } else {
            Err(ToolError::UnknownTool(call.name.clone()))
        }
    }
}

/// Single registry that dispatches to static tools first, then per-claim queue tools.
/// When a scratchpad mutex is provided, `working_memory_read` and `working_memory_update`
/// are intercepted to operate on the shared in-memory scratchpad instead of going through
/// the disk-based `MemoryToolRegistry`.
pub struct ApexToolRegistry {
    pub static_tools: Arc<CompositeToolRegistry>,
    pub queue_tools: QueueToolRegistry,
    pub scratchpad: Option<Arc<Mutex<Scratchpad>>>,
    pub memory: Option<Arc<dyn WorkingMemory>>,
}

#[async_trait]
impl ToolRegistry for ApexToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        let mut defs = self.static_tools.definitions();
        defs.extend(self.queue_tools.definitions());
        // compact_conversation is always available — execution is handled
        // by the agentic loop which has access to messages and the LLM.
        defs.push(compact_conversation_def());
        defs
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        // compact_conversation should never reach here — the agentic loop intercepts it.
        if call.name == COMPACT_CONVERSATION_TOOL {
            return Err(ToolError::Execution(
                "compact_conversation must be handled by the agentic loop".into(),
            ));
        }

        // Intercept working memory tools to use the shared mutex
        if let Some(ref pad_arc) = self.scratchpad {
            match call.name.as_str() {
                "working_memory_read" => {
                    let pad = pad_arc.lock().await;
                    return Ok(ToolResult {
                        tool_use_id: call.id.clone(),
                        name: call.name.clone(),
                        output: serde_json::json!({ "content": pad.to_markdown() }),
                        is_error: false,
                        ..Default::default()
                    });
                }
                "working_memory_update" => {
                    let mut pad = pad_arc.lock().await;
                    apply_wm_update(&mut pad, call)?;
                    // Persist to disk for durability
                    if let Some(ref mem) = self.memory {
                        let _ = mem.save(&pad).await;
                    }
                    return Ok(ToolResult {
                        tool_use_id: call.id.clone(),
                        name: call.name.clone(),
                        output: serde_json::json!({ "content": pad.to_markdown() }),
                        is_error: false,
                        ..Default::default()
                    });
                }
                _ => {}
            }
        }

        if self.static_tools.by_name.contains_key(&call.name) {
            return self.static_tools.execute(call).await;
        }
        self.queue_tools.execute(call).await
    }
}

/// Apply working_memory_update mutations to a scratchpad (same logic as MemoryToolRegistry).
fn apply_wm_update(pad: &mut Scratchpad, call: &ToolCall) -> Result<(), ToolError> {
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
            return Err(ToolError::InvalidInput(format!("subtask index {index} not found")));
        }
    }
    Ok(())
}

/// Owned filtered view of a ToolRegistry — filters by allowed tool names.
pub struct OwnedFilteredToolRegistry {
    inner: Arc<CompositeToolRegistry>,
    allowed: HashSet<String>,
}

impl OwnedFilteredToolRegistry {
    pub fn new(inner: Arc<CompositeToolRegistry>, allowed: HashSet<String>) -> Self {
        Self { inner, allowed }
    }
}

#[async_trait]
impl ToolRegistry for OwnedFilteredToolRegistry {
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

fn compact_conversation_def() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: COMPACT_CONVERSATION_TOOL.into(),
            description: "Summarize older conversation messages to free up context window space. \
                Call this when you notice the conversation is getting long. \
                Recent turns are preserved intact; older messages are replaced \
                with a concise LLM-generated summary.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    }
}

/// Build the static tool registries (Builtin, Memory, Custom, Config, Delegate) once per process.
pub fn build_static_tools(
    paths: &ProjectPaths,
    memory: Arc<dyn WorkingMemory>,
    long_term: Arc<dyn MemoryStore>,
    skills: Arc<dyn SkillStore>,
    invariants: Arc<Invariants>,
    spawner: Arc<dyn SubAgentSpawner>,
    roles: Arc<[RoleProfile]>,
    remaining_delegate_depth: u32,
) -> Arc<CompositeToolRegistry> {
    let memory_tools = MemoryToolRegistry::new(memory, long_term.clone(), skills.clone());
    let custom_spill = SpillManager::new(paths.scratch_dir.clone());
    let custom_tools = CustomToolRegistry::new(
        paths.tools_dir.clone(),
        custom_spill,
        Some(skills),
    );
    let config_tools = ConfigToolRegistry::new(paths.config_dir.clone(), Arc::clone(&invariants));
    let delegate_tools = DelegateToolRegistry::new(
        roles,
        paths.prompts_dir.clone(),
        spawner,
        remaining_delegate_depth,
    );
    Arc::new(CompositeToolRegistry::new(vec![
        Box::new(BuiltinToolRegistry::new(paths.scratch_dir.clone())),
        Box::new(memory_tools),
        Box::new(custom_tools),
        Box::new(config_tools),
        Box::new(delegate_tools),
    ]))
}
