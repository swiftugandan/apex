use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use apex_core::domain::{ToolCall, ToolDef, ToolResult};
use apex_core::error::ToolError;
use apex_core::ports::ToolRegistry;
use serde_json::Value;

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

    fn rewrite_input(
        &self,
        call: &ToolCall,
        result: &ToolResult,
        max_bytes: usize,
    ) -> Option<Value> {
        self.by_name
            .get(&call.name)
            .and_then(|&idx| self.registries[idx].rewrite_input(call, result, max_bytes))
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

    fn rewrite_input(
        &self,
        call: &ToolCall,
        result: &ToolResult,
        max_bytes: usize,
    ) -> Option<Value> {
        if self.allowed.contains(&call.name) {
            self.inner.rewrite_input(call, result, max_bytes)
        } else {
            None
        }
    }
}
