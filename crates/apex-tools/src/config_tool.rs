use std::path::PathBuf;
use std::sync::Arc;

use apex_core::config::{validate_against_invariants, AgentConfig, ConfigLoader, Invariants};
use apex_core::domain::{ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use apex_core::ports::ToolRegistry;
use async_trait::async_trait;
use serde_json::{json, Value};

/// Tool registry providing the `update_config` tool for runtime self-modification.
pub struct ConfigToolRegistry {
    config_dir: PathBuf,
    invariants: Arc<Invariants>,
}

impl ConfigToolRegistry {
    pub fn new(config_dir: PathBuf, invariants: Arc<Invariants>) -> Self {
        Self {
            config_dir,
            invariants,
        }
    }
}

#[async_trait]
impl ToolRegistry for ConfigToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            schema: ToolSchema {
                name: "update_config".to_string(),
                description: "Read or update the agent configuration (agent.toml). Use action='read' to view current config, action='update' with changes to modify it. Changes are validated against operator invariants.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["read", "update"],
                            "description": "Whether to read or update the configuration"
                        },
                        "changes": {
                            "type": "object",
                            "description": "Partial agent.toml structure with values to change (only for action=update)"
                        }
                    },
                    "required": ["action"]
                }),
            },
        }]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if call.name != "update_config" {
            return Err(ToolError::UnknownTool(call.name.clone()));
        }

        let action = call
            .input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match action {
            "read" => self.handle_read(call),
            "update" => self.handle_update(call),
            other => Err(ToolError::InvalidInput(format!(
                "unknown action: '{}'. Expected 'read' or 'update'.",
                other
            ))),
        }
    }
}

use crate::tool_result_helpers::{ok_result, err_result};

impl ConfigToolRegistry {
    fn handle_read(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let config = ConfigLoader::load_agent_config(&self.config_dir)
            .map_err(|e| ToolError::Execution(format!("failed to load config: {e}")))?;

        let toml_str = config
            .to_toml()
            .map_err(|e| ToolError::Execution(format!("failed to serialize config: {e}")))?;

        ok_result(call, json!({ "config": toml_str }))
    }

    fn handle_update(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let changes = match call.input.get("changes") {
            Some(c) if c.is_object() => c,
            Some(_) => return err_result(call, "\"changes\" must be an object"),
            None => {
                return err_result(call, "\"changes\" field is required for action=update")
            }
        };

        let current = ConfigLoader::load_agent_config(&self.config_dir)
            .map_err(|e| ToolError::Execution(format!("failed to load config: {e}")))?;

        let current_toml = current
            .to_toml()
            .map_err(|e| ToolError::Execution(format!("failed to serialize config: {e}")))?;

        let mut base: Value = toml::from_str(&current_toml)
            .map_err(|e| ToolError::Execution(format!("failed to parse config: {e}")))?;

        deep_merge(&mut base, changes);

        let merged_toml = toml::to_string_pretty(&base)
            .map_err(|e| ToolError::Execution(format!("failed to serialize merged: {e}")))?;

        let merged_config = AgentConfig::from_toml(&merged_toml)
            .map_err(|e| ToolError::Execution(format!("invalid config after merge: {e}")))?;

        let report = validate_against_invariants(&merged_config, &self.invariants);
        if !report.is_ok() {
            return err_result(
                call,
                &format!(
                    "config update rejected — invariant violations:\n{}",
                    report.display()
                ),
            );
        }

        ConfigLoader::save_agent_config(&self.config_dir, &merged_config)
            .map_err(|e| ToolError::Execution(format!("failed to write config: {e}")))?;

        ok_result(
            call,
            json!({
                "status": "updated",
                "config": merged_toml
            }),
        )
    }
}

/// Deep merge: recursively walk nested objects, replace leaf values.
fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_val) in overlay_map {
                let base_val = base_map.entry(key.clone()).or_insert(Value::Null);
                deep_merge(base_val, overlay_val);
            }
        }
        (base, overlay) => {
            *base = overlay.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, ConfigToolRegistry) {
        let dir = TempDir::new().unwrap();
        ConfigLoader::write_default_agent_config(dir.path()).unwrap();
        ConfigLoader::write_default_invariants(dir.path()).unwrap();
        let inv = ConfigLoader::load_invariants(dir.path()).unwrap();
        let registry = ConfigToolRegistry::new(dir.path().to_path_buf(), Arc::new(inv));
        (dir, registry)
    }

    fn make_call(name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: "test-id".to_string(),
            name: name.to_string(),
            input,
        }
    }

    #[tokio::test]
    async fn read_returns_valid_toml() {
        let (_dir, registry) = setup();
        let call = make_call("update_config", json!({"action": "read"}));
        let result = registry.execute(&call).await.unwrap();
        assert!(!result.is_error);
        let config_str = result.output["config"].as_str().unwrap();
        let _: AgentConfig = toml::from_str(config_str).expect("invalid TOML in read result");
    }

    #[tokio::test]
    async fn update_with_valid_changes() {
        let (dir, registry) = setup();
        let call = make_call(
            "update_config",
            json!({
                "action": "update",
                "changes": {
                    "agent": {
                        "max_depth": 4
                    }
                }
            }),
        );
        let result = registry.execute(&call).await.unwrap();
        assert!(!result.is_error, "update failed: {:?}", result.output);

        let config = ConfigLoader::load_agent_config(dir.path()).unwrap();
        assert_eq!(config.agent.max_depth, 4);
    }

    #[tokio::test]
    async fn update_exceeding_invariant_returns_error() {
        let (_dir, registry) = setup();
        let call = make_call(
            "update_config",
            json!({
                "action": "update",
                "changes": {
                    "agent": {
                        "max_depth": 100
                    }
                }
            }),
        );
        let result = registry.execute(&call).await.unwrap();
        assert!(result.is_error);
        let err = result.output["error"].as_str().unwrap();
        assert!(err.contains("invariant violations"));
    }

    #[test]
    fn deep_merge_nested() {
        let mut base: Value = serde_json::from_str(r#"{"a": {"b": 1, "c": 2}, "d": 3}"#).unwrap();
        let overlay: Value = serde_json::from_str(r#"{"a": {"b": 10}}"#).unwrap();
        deep_merge(&mut base, &overlay);
        assert_eq!(base["a"]["b"], 10);
        assert_eq!(base["a"]["c"], 2);
        assert_eq!(base["d"], 3);
    }
}
