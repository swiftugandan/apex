use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use apex_core::config::{Invariants, RoleProfile};
use apex_core::domain::{ToolCall, ToolDef, ToolResult};
use apex_core::error::ToolError;
use apex_core::ports::{MemoryStore, ToolRegistry, WorkingMemory};

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
    cached_defs: Vec<ToolDef>,
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
            cached_defs,
        }
    }
}

#[async_trait]
impl ToolRegistry for CompositeToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        self.cached_defs.clone()
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
pub struct ApexToolRegistry {
    pub static_tools: Arc<CompositeToolRegistry>,
    pub queue_tools: QueueToolRegistry,
}

#[async_trait]
impl ToolRegistry for ApexToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        let mut defs = self.static_tools.definitions();
        defs.extend(self.queue_tools.definitions());
        defs
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if self.static_tools.by_name.contains_key(&call.name) {
            return self.static_tools.execute(call).await;
        }
        self.queue_tools.execute(call).await
    }
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

/// Build the static tool registries (Builtin, Memory, Custom, Config, Delegate) once per process.
pub fn build_static_tools(
    paths: &ProjectPaths,
    memory: Arc<dyn WorkingMemory>,
    long_term: Arc<dyn MemoryStore>,
    invariants: Arc<Invariants>,
    spawner: Arc<dyn SubAgentSpawner>,
    _max_tool_result_bytes: usize,
    roles: Vec<RoleProfile>,
    remaining_delegate_depth: u32,
) -> Arc<CompositeToolRegistry> {
    let memory_tools = MemoryToolRegistry::new(memory, long_term.clone());
    let custom_spill = SpillManager::new(paths.scratch_dir.clone());
    let custom_tools = CustomToolRegistry::new(
        paths.tools_dir.clone(),
        custom_spill,
        Some(long_term.clone()),
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
