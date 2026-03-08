use std::path::PathBuf;

use apex_core::domain::{ToolCall, ToolDef, ToolResult};
use apex_core::error::ToolError;
use apex_core::ports::ToolRegistry;
use async_trait::async_trait;

use super::file_read;
use super::file_write;
use super::shell_exec;
use super::spill::SpillManager;

pub struct BuiltinToolRegistry {
    spill: SpillManager,
}

impl Default for BuiltinToolRegistry {
    fn default() -> Self {
        Self::new(std::env::temp_dir().join("apex-scratch"))
    }
}

impl BuiltinToolRegistry {
    pub fn new(scratch_dir: PathBuf) -> Self {
        Self {
            spill: SpillManager::new(scratch_dir),
        }
    }

    pub fn spill_manager(&self) -> &SpillManager {
        &self.spill
    }
}

#[async_trait]
impl ToolRegistry for BuiltinToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![
            shell_exec::definition(),
            file_read::definition(),
            file_write::definition(),
        ]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        match call.name.as_str() {
            "shell_exec" => shell_exec::execute(call, &self.spill).await,
            "file_read" => file_read::execute(call).await,
            "file_write" => file_write::execute(call).await,
            _ => Err(ToolError::UnknownTool(call.name.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn definitions_returns_3_tools() {
        let registry = BuiltinToolRegistry::default();
        let defs = registry.definitions();
        assert_eq!(defs.len(), 3);
        let names: Vec<&str> = defs.iter().map(|d| d.schema.name.as_str()).collect();
        assert!(names.contains(&"shell_exec"));
        assert!(names.contains(&"file_read"));
        assert!(names.contains(&"file_write"));
    }

    #[test]
    fn schemas_returns_3_schemas() {
        let registry = BuiltinToolRegistry::default();
        let schemas = registry.schemas();
        assert_eq!(schemas.len(), 3);
        let defs = registry.definitions();
        for (schema, def) in schemas.iter().zip(defs.iter()) {
            assert_eq!(schema.name, def.schema.name);
        }
    }

    #[tokio::test]
    async fn unknown_tool() {
        let registry = BuiltinToolRegistry::default();
        let call = ToolCall {
            id: "test-id".into(),
            name: "nonexistent".into(),
            input: json!({}),
        };
        let err = registry.execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(_)));
    }
}
