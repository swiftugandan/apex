use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use apex_core::config::ToolLoadingSection;
use apex_core::domain::{ToolCall, ToolDef, ToolLoading, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use apex_core::ports::ToolRegistry;
use serde_json::{json, Value};

// ── Meta-tool name ────────────────────────────────────────────────

const LOAD_TOOL_DEFINITIONS: &str = "load_tool_definitions";

// ── CompositeToolRegistry ──────────────────────────────────────────

/// Aggregates multiple tool registries with O(1) dispatch via HashMap.
/// Supports two-tier loading: eager tools send full schemas every turn,
/// deferred tools send stubs until loaded via `load_tool_definitions`.
pub struct CompositeToolRegistry {
    by_name: HashMap<String, usize>,
    /// Maps tool name → index in `cached_defs` for O(1) def lookup.
    def_by_name: HashMap<String, usize>,
    registries: Vec<Box<dyn ToolRegistry>>,
    cached_defs: Arc<Vec<ToolDef>>,
    /// Tracks which deferred tools have been loaded this session.
    loaded_deferred: RwLock<HashSet<String>>,
    /// Whether any deferred tools exist (avoids lock contention when none).
    has_deferred: bool,
}

impl CompositeToolRegistry {
    pub fn new(registries: Vec<Box<dyn ToolRegistry>>) -> Self {
        Self::with_config(registries, &ToolLoadingSection::default())
    }

    pub fn with_config(
        registries: Vec<Box<dyn ToolRegistry>>,
        tool_loading: &ToolLoadingSection,
    ) -> Self {
        let mut by_name = HashMap::new();
        let mut def_by_name = HashMap::new();
        let mut cached_defs = Vec::new();
        let mut has_deferred = false;

        for (idx, registry) in registries.iter().enumerate() {
            for mut def in registry.definitions() {
                // Apply config overrides
                if tool_loading.disable_deferred || tool_loading.eager.contains(&def.schema.name) {
                    def.loading = ToolLoading::Eager;
                } else if tool_loading.deferred.contains(&def.schema.name) {
                    def.loading = ToolLoading::Deferred;
                }

                if def.loading == ToolLoading::Deferred {
                    has_deferred = true;
                }

                by_name.insert(def.schema.name.clone(), idx);
                def_by_name.insert(def.schema.name.clone(), cached_defs.len());
                cached_defs.push(def);
            }
        }

        Self {
            by_name,
            def_by_name,
            registries,
            cached_defs: Arc::new(cached_defs),
            loaded_deferred: RwLock::new(HashSet::new()),
            has_deferred,
        }
    }

    /// Return schemas with deferred tools as stubs unless already loaded.
    fn effective_schemas(&self) -> Vec<ToolSchema> {
        if !self.has_deferred {
            return self.cached_defs.iter().map(|d| d.schema.clone()).collect();
        }

        let loaded = self.loaded_deferred.read();
        let mut schemas: Vec<ToolSchema> = self
            .cached_defs
            .iter()
            .map(|def| {
                if def.loading == ToolLoading::Deferred && !loaded.contains(&def.schema.name) {
                    // Stub schema — minimal, tells LLM to call load_tool_definitions
                    ToolSchema {
                        name: def.schema.name.clone(),
                        description: format!(
                            "[Deferred] {}. Call load_tool_definitions to load full schema.",
                            def.schema.description
                        ),
                        input_schema: json!({"type": "object", "properties": {}}),
                    }
                } else {
                    def.schema.clone()
                }
            })
            .collect();

        // Always include the meta-tool
        schemas.push(Self::meta_tool_schema());
        schemas
    }

    /// Mark tools as loaded, returning their full schemas.
    fn load_tools(&self, names: &[String]) -> Value {
        let mut loaded = self.loaded_deferred.write();
        let mut results = Vec::new();

        for name in names {
            if let Some(def) = self
                .def_by_name
                .get(name)
                .map(|&idx| &self.cached_defs[idx])
            {
                let status = if def.loading == ToolLoading::Deferred {
                    loaded.insert(name.clone());
                    "loaded"
                } else {
                    "already_eager"
                };
                results.push(json!({
                    "name": def.schema.name,
                    "description": def.schema.description,
                    "input_schema": def.schema.input_schema,
                    "status": status
                }));
            } else {
                results.push(json!({
                    "name": name,
                    "status": "not_found"
                }));
            }
        }

        json!({ "tools": results })
    }

    /// Auto-load a deferred tool (graceful fallback when LLM calls without loading first).
    fn auto_load(&self, name: &str) -> bool {
        if let Some(def) = self
            .def_by_name
            .get(name)
            .map(|&idx| &self.cached_defs[idx])
        {
            if def.loading == ToolLoading::Deferred {
                let mut loaded = self.loaded_deferred.write();
                loaded.insert(name.to_string());
                return true;
            }
        }
        false
    }

    fn meta_tool_schema() -> ToolSchema {
        ToolSchema {
            name: LOAD_TOOL_DEFINITIONS.into(),
            description: "Load the full input schemas for deferred tools. Call this before using a deferred tool to get its full parameter schema.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of tool names to load"
                    }
                },
                "required": ["tools"]
            }),
        }
    }
}

#[async_trait]
impl ToolRegistry for CompositeToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        if self.has_deferred {
            self.cached_defs
                .iter()
                .cloned()
                .chain(std::iter::once(ToolDef::eager(Self::meta_tool_schema())))
                .collect()
        } else {
            self.cached_defs.to_vec()
        }
    }

    fn schemas(&self) -> Vec<ToolSchema> {
        self.effective_schemas()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        // Handle the meta-tool
        if call.name == LOAD_TOOL_DEFINITIONS {
            let tools: Vec<String> = call
                .input
                .get("tools")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let output = self.load_tools(&tools);
            return Ok(ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                output,
                is_error: false,
                ..Default::default()
            });
        }

        if let Some(&idx) = self.by_name.get(&call.name) {
            // Auto-load deferred tools if called without explicit loading
            self.auto_load(&call.name);
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
        if call.name == LOAD_TOOL_DEFINITIONS {
            return None;
        }
        self.by_name
            .get(&call.name)
            .and_then(|&idx| self.registries[idx].rewrite_input(call, result, max_bytes))
    }
}

/// Owned filtered view of a ToolRegistry — filters by allowed tool names.
/// The `load_tool_definitions` meta-tool is always allowed when deferred tools exist.
pub struct OwnedFilteredToolRegistry {
    inner: Arc<CompositeToolRegistry>,
    allowed: HashSet<String>,
}

impl OwnedFilteredToolRegistry {
    pub fn new(inner: Arc<CompositeToolRegistry>, allowed: HashSet<String>) -> Self {
        Self { inner, allowed }
    }

    fn is_allowed(&self, name: &str) -> bool {
        self.allowed.contains(name) || (self.inner.has_deferred && name == LOAD_TOOL_DEFINITIONS)
    }
}

#[async_trait]
impl ToolRegistry for OwnedFilteredToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|d| self.is_allowed(&d.schema.name))
            .collect()
    }

    fn schemas(&self) -> Vec<ToolSchema> {
        self.inner
            .schemas()
            .into_iter()
            .filter(|s| self.is_allowed(&s.name))
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if !self.is_allowed(&call.name) {
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
        if self.is_allowed(&call.name) {
            self.inner.rewrite_input(call, result, max_bytes)
        } else {
            None
        }
    }
}
