use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};

use apex_core::domain::{
    HookAction, HookActionType, HookDef, HookEvent, HookFilter, HookMeta, OnFailureBehavior,
    ToolCall, ToolDef, ToolLoading, ToolResult, ToolSchema,
};
use apex_core::error::ToolError;
use apex_core::ports::{HookRegistry, ToolRegistry};

use crate::hooks::FsHookRegistry;

/// Tool registry that exposes a `manage_hooks` tool for CRUD operations on lifecycle hooks.
pub struct HooksToolRegistry {
    hooks_dir: PathBuf,
}

impl HooksToolRegistry {
    pub fn new(hooks_dir: PathBuf) -> Self {
        Self { hooks_dir }
    }

    fn list_hooks(&self) -> Result<Value, ToolError> {
        let registry = FsHookRegistry::new(self.hooks_dir.clone());
        let hooks = registry.all_hooks();

        let entries: Vec<Value> = hooks
            .iter()
            .map(|h| {
                json!({
                    "name": h.hook.name,
                    "event": h.hook.event.to_string(),
                    "priority": h.hook.priority,
                    "type": format!("{:?}", h.action.action_type).to_lowercase(),
                    "filter": h.hook.filter.tool.as_deref().unwrap_or("*"),
                    "invariant": h.hook.invariant,
                    "source": h.source_path,
                })
            })
            .collect();

        Ok(json!({ "hooks": entries, "count": entries.len() }))
    }

    fn show_hook(&self, name: &str) -> Result<Value, ToolError> {
        let registry = FsHookRegistry::new(self.hooks_dir.clone());
        let hooks = registry.all_hooks();

        let hook = hooks
            .iter()
            .find(|h| h.hook.name == name)
            .ok_or_else(|| ToolError::InvalidInput(format!("hook '{name}' not found")))?;

        // Read the raw TOML for display
        let toml_content = hook
            .source_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_else(|| "(source not available)".to_string());

        Ok(json!({
            "name": hook.hook.name,
            "event": hook.hook.event.to_string(),
            "priority": hook.hook.priority,
            "action_type": format!("{:?}", hook.action.action_type).to_lowercase(),
            "filter": hook.hook.filter.tool,
            "invariant": hook.hook.invariant,
            "source_path": hook.source_path,
            "toml": toml_content,
        }))
    }

    fn create_hook(&self, input: &Value) -> Result<Value, ToolError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("name is required".into()))?;

        let event_str = input["event"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("event is required".into()))?;

        let event: HookEvent = event_str
            .parse()
            .map_err(|e: String| ToolError::InvalidInput(e))?;

        let action_type_str = input["action_type"].as_str().unwrap_or("script");

        let action_type = match action_type_str {
            "script" => HookActionType::Script,
            "transform" => HookActionType::Transform,
            "block" => HookActionType::Block,
            "inject" => HookActionType::Inject,
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "unknown action type: {other}"
                )))
            }
        };

        let priority = input["priority"].as_i64().unwrap_or(50) as i32;

        let filter = HookFilter {
            tool: input["filter_tool"].as_str().map(String::from),
        };

        let on_failure_str = input["on_failure"].as_str().unwrap_or("warn");
        let on_failure = match on_failure_str {
            "block" => OnFailureBehavior::Block,
            "continue" => OnFailureBehavior::Continue,
            _ => OnFailureBehavior::Warn,
        };

        let hook_def = HookDef {
            hook: HookMeta {
                name: name.to_string(),
                event,
                filter,
                priority,
                invariant: false,
                propagate: input["propagate"].as_bool().unwrap_or(false),
            },
            action: HookAction {
                action_type,
                command: input["command"].as_str().map(String::from),
                input: input["input"].as_str().map(String::from),
                timeout_ms: input["timeout_ms"].as_u64().unwrap_or(5000),
                content: input["content"].as_str().map(String::from),
                on_failure,
                message: input["message"].as_str().map(String::from),
            },
            source_path: None,
        };

        // Validate
        FsHookRegistry::validate_hook(&hook_def)
            .map_err(|errs| ToolError::InvalidInput(errs.join("; ")))?;

        // Write to event directory
        let event_dir = self.hooks_dir.join(format!("{}.d", event));
        std::fs::create_dir_all(&event_dir)
            .map_err(|e| ToolError::Execution(format!("failed to create hook directory: {e}")))?;

        let hook_path = event_dir.join(format!("{name}.toml"));

        // Don't overwrite existing hooks
        if hook_path.exists() {
            return Err(ToolError::InvalidInput(format!(
                "hook '{name}' already exists at {}. Use the 'edit' action to modify it.",
                hook_path.display()
            )));
        }

        let toml_str = toml::to_string_pretty(&hook_def)
            .map_err(|e| ToolError::Execution(format!("failed to serialize hook: {e}")))?;

        std::fs::write(&hook_path, &toml_str)
            .map_err(|e| ToolError::Execution(format!("failed to write hook file: {e}")))?;

        Ok(json!({
            "created": true,
            "path": hook_path.to_string_lossy(),
            "name": name,
            "event": event_str,
        }))
    }

    fn edit_hook(&self, input: &Value) -> Result<Value, ToolError> {
        let name = input["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("name is required".into()))?;

        // Find the existing hook
        let registry = FsHookRegistry::new(self.hooks_dir.clone());
        let hooks = registry.all_hooks();
        let existing = hooks
            .iter()
            .find(|h| h.hook.name == name)
            .ok_or_else(|| ToolError::InvalidInput(format!("hook '{name}' not found")))?;

        if existing.hook.invariant {
            return Err(ToolError::InvalidInput(format!(
                "hook '{name}' is marked as invariant and cannot be edited"
            )));
        }

        let source_path = existing
            .source_path
            .as_ref()
            .ok_or_else(|| ToolError::Execution("hook has no source path".into()))?
            .clone();

        // Build an updated HookDef, using existing values as defaults
        let event = if let Some(event_str) = input["event"].as_str() {
            event_str
                .parse()
                .map_err(|e: String| ToolError::InvalidInput(e))?
        } else {
            existing.hook.event
        };

        let action_type = if let Some(at) = input["action_type"].as_str() {
            match at {
                "script" => HookActionType::Script,
                "transform" => HookActionType::Transform,
                "block" => HookActionType::Block,
                "inject" => HookActionType::Inject,
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "unknown action type: {other}"
                    )))
                }
            }
        } else {
            existing.action.action_type
        };

        let priority = input["priority"]
            .as_i64()
            .map(|v| v as i32)
            .unwrap_or(existing.hook.priority);

        let filter = HookFilter {
            tool: input["filter_tool"]
                .as_str()
                .map(String::from)
                .or_else(|| existing.hook.filter.tool.clone()),
        };

        let on_failure = if let Some(of) = input["on_failure"].as_str() {
            match of {
                "block" => OnFailureBehavior::Block,
                "continue" => OnFailureBehavior::Continue,
                _ => OnFailureBehavior::Warn,
            }
        } else {
            existing.action.on_failure
        };

        let hook_def = HookDef {
            hook: HookMeta {
                name: name.to_string(),
                event,
                filter,
                priority,
                invariant: false,
                propagate: input["propagate"]
                    .as_bool()
                    .unwrap_or(existing.hook.propagate),
            },
            action: HookAction {
                action_type,
                command: input["command"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| existing.action.command.clone()),
                input: input["input"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| existing.action.input.clone()),
                timeout_ms: input["timeout_ms"]
                    .as_u64()
                    .unwrap_or(existing.action.timeout_ms),
                content: input["content"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| existing.action.content.clone()),
                on_failure,
                message: input["message"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| existing.action.message.clone()),
            },
            source_path: None,
        };

        // Validate the updated hook
        FsHookRegistry::validate_hook(&hook_def)
            .map_err(|errs| ToolError::InvalidInput(errs.join("; ")))?;

        let toml_str = toml::to_string_pretty(&hook_def)
            .map_err(|e| ToolError::Execution(format!("failed to serialize hook: {e}")))?;

        // If the event changed, we need to move the file to a new event directory
        let new_event_dir = self.hooks_dir.join(format!("{}.d", event));
        std::fs::create_dir_all(&new_event_dir)
            .map_err(|e| ToolError::Execution(format!("failed to create hook directory: {e}")))?;
        let new_path = new_event_dir.join(format!("{name}.toml"));

        // Write new file first, then remove old if path changed
        std::fs::write(&new_path, &toml_str)
            .map_err(|e| ToolError::Execution(format!("failed to write hook file: {e}")))?;

        let source = PathBuf::from(&source_path);
        if source != new_path && source.exists() {
            let _ = std::fs::remove_file(&source);
        }

        Ok(json!({
            "edited": true,
            "path": new_path.to_string_lossy(),
            "name": name,
            "event": event.to_string(),
        }))
    }

    fn delete_hook(&self, name: &str) -> Result<Value, ToolError> {
        let registry = FsHookRegistry::new(self.hooks_dir.clone());
        let hooks = registry.all_hooks();

        let hook = hooks
            .iter()
            .find(|h| h.hook.name == name)
            .ok_or_else(|| ToolError::InvalidInput(format!("hook '{name}' not found")))?;

        if hook.hook.invariant {
            return Err(ToolError::InvalidInput(format!(
                "hook '{name}' is marked as invariant and cannot be deleted"
            )));
        }

        let path = hook
            .source_path
            .as_ref()
            .ok_or_else(|| ToolError::Execution("hook has no source path".into()))?;

        std::fs::remove_file(path)
            .map_err(|e| ToolError::Execution(format!("failed to delete hook file: {e}")))?;

        Ok(json!({
            "deleted": true,
            "name": name,
            "path": path,
        }))
    }

    fn validate_hooks(&self) -> Result<Value, ToolError> {
        let registry = FsHookRegistry::new(self.hooks_dir.clone());
        let issues = registry.validate_all();

        if issues.is_empty() {
            Ok(json!({ "valid": true, "message": "All hooks are valid." }))
        } else {
            let issue_list: Vec<Value> = issues
                .iter()
                .map(|(name, errs)| json!({ "hook": name, "errors": errs }))
                .collect();
            Ok(json!({
                "valid": false,
                "issues": issue_list,
            }))
        }
    }
}

#[async_trait]
impl ToolRegistry for HooksToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            schema: ToolSchema {
                name: "manage_hooks".into(),
                description: "Manage lifecycle hooks. Hooks are declarative TOML files in \
                    .apex/hooks/ that fire at lifecycle events (before_turn, before_tool_call, \
                    after_tool_result, etc.). Actions: list, show, create, edit, delete, validate."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "show", "create", "edit", "delete", "validate"],
                            "description": "The operation to perform."
                        },
                        "name": {
                            "type": "string",
                            "description": "Hook name (required for show, create, delete)."
                        },
                        "event": {
                            "type": "string",
                            "enum": ["before_turn", "after_turn", "before_tool_call",
                                     "after_tool_result", "before_push", "after_claim",
                                     "on_success", "on_failure"],
                            "description": "Lifecycle event (required for create)."
                        },
                        "action_type": {
                            "type": "string",
                            "enum": ["script", "transform", "block", "inject"],
                            "description": "What the hook does (required for create)."
                        },
                        "command": {
                            "type": "string",
                            "description": "Shell command to run (for script/transform/inject types)."
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to inject (for inject type)."
                        },
                        "message": {
                            "type": "string",
                            "description": "Block message (for block type)."
                        },
                        "filter_tool": {
                            "type": "string",
                            "description": "Only fire for this tool name (optional)."
                        },
                        "priority": {
                            "type": "integer",
                            "description": "Execution priority (lower = first, default 50)."
                        },
                        "input": {
                            "type": "string",
                            "description": "What to pipe to stdin: tool_call, tool_result, message, context."
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "description": "Script timeout in milliseconds (default 5000)."
                        },
                        "on_failure": {
                            "type": "string",
                            "enum": ["block", "warn", "continue"],
                            "description": "What to do if the script fails (default warn)."
                        }
                    },
                    "required": ["action"]
                }),
            },
            loading: ToolLoading::Deferred,
        }]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let action = call.input["action"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("action is required".into()))?;

        let output = match action {
            "list" => self.list_hooks()?,
            "show" => {
                let name = call.input["name"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidInput("name is required for show".into()))?;
                self.show_hook(name)?
            }
            "create" => self.create_hook(&call.input)?,
            "edit" => self.edit_hook(&call.input)?,
            "delete" => {
                let name = call.input["name"]
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidInput("name is required for delete".into()))?;
                self.delete_hook(name)?
            }
            "validate" => self.validate_hooks()?,
            other => {
                return Err(ToolError::InvalidInput(format!(
                    "unknown action: {other}. Use list, show, create, edit, delete, or validate."
                )))
            }
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
    use tempfile::TempDir;

    fn make_call(action: &str, extra: Value) -> ToolCall {
        let mut input = json!({ "action": action });
        if let Value::Object(map) = extra {
            for (k, v) in map {
                input[k] = v;
            }
        }
        ToolCall {
            id: "test-1".into(),
            name: "manage_hooks".into(),
            input,
        }
    }

    #[tokio::test]
    async fn list_empty() {
        let dir = TempDir::new().unwrap();
        let registry = HooksToolRegistry::new(dir.path().join("hooks"));
        let call = make_call("list", json!({}));
        let result = registry.execute(&call).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output["count"], 0);
    }

    #[tokio::test]
    async fn create_and_list() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let registry = HooksToolRegistry::new(hooks_dir.clone());

        // Create a hook
        let call = make_call(
            "create",
            json!({
                "name": "test_hook",
                "event": "before_turn",
                "action_type": "inject",
                "content": "Test injection"
            }),
        );
        let result = registry.execute(&call).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output["created"], true);

        // List hooks
        let call = make_call("list", json!({}));
        let result = registry.execute(&call).await.unwrap();
        assert_eq!(result.output["count"], 1);
    }

    #[tokio::test]
    async fn create_show_delete() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let registry = HooksToolRegistry::new(hooks_dir);

        // Create
        let call = make_call(
            "create",
            json!({
                "name": "my_hook",
                "event": "before_tool_call",
                "action_type": "script",
                "command": "echo safety check",
                "filter_tool": "shell_exec",
                "priority": 10,
            }),
        );
        let result = registry.execute(&call).await.unwrap();
        assert!(!result.is_error);

        // Show
        let call = make_call("show", json!({ "name": "my_hook" }));
        let result = registry.execute(&call).await.unwrap();
        assert_eq!(result.output["name"], "my_hook");
        assert_eq!(result.output["event"], "before_tool_call");

        // Delete
        let call = make_call("delete", json!({ "name": "my_hook" }));
        let result = registry.execute(&call).await.unwrap();
        assert_eq!(result.output["deleted"], true);

        // Verify gone
        let call = make_call("show", json!({ "name": "my_hook" }));
        let result = registry.execute(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cannot_delete_invariant_hook() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let event_dir = hooks_dir.join("before_tool_call.d");
        std::fs::create_dir_all(&event_dir).unwrap();

        // Manually create an invariant hook
        std::fs::write(
            event_dir.join("safety.toml"),
            r#"
[hook]
name = "safety"
event = "before_tool_call"
invariant = true

[action]
type = "script"
command = "echo check"
"#,
        )
        .unwrap();

        let registry = HooksToolRegistry::new(hooks_dir);
        let call = make_call("delete", json!({ "name": "safety" }));
        let result = registry.execute(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn validate_hooks() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let registry = HooksToolRegistry::new(hooks_dir);

        let call = make_call("validate", json!({}));
        let result = registry.execute(&call).await.unwrap();
        assert_eq!(result.output["valid"], true);
    }

    #[tokio::test]
    async fn edit_hook_updates_fields() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let registry = HooksToolRegistry::new(hooks_dir.clone());

        // Create a hook first
        let call = make_call(
            "create",
            json!({
                "name": "editable",
                "event": "before_turn",
                "action_type": "inject",
                "content": "original content",
                "priority": 50,
            }),
        );
        let result = registry.execute(&call).await.unwrap();
        assert_eq!(result.output["created"], true);

        // Edit the hook — change content and priority
        let call = make_call(
            "edit",
            json!({
                "name": "editable",
                "content": "updated content",
                "priority": 10,
            }),
        );
        let result = registry.execute(&call).await.unwrap();
        assert_eq!(result.output["edited"], true);

        // Show the hook and verify changes
        let call = make_call("show", json!({ "name": "editable" }));
        let result = registry.execute(&call).await.unwrap();
        assert_eq!(result.output["priority"], 10);
        let toml_str = result.output["toml"].as_str().unwrap();
        assert!(toml_str.contains("updated content"));
    }

    #[tokio::test]
    async fn edit_nonexistent_hook_fails() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let registry = HooksToolRegistry::new(hooks_dir);

        let call = make_call(
            "edit",
            json!({
                "name": "ghost",
                "content": "new content",
            }),
        );
        let result = registry.execute(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_invariant_hook_fails() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let event_dir = hooks_dir.join("before_tool_call.d");
        std::fs::create_dir_all(&event_dir).unwrap();

        std::fs::write(
            event_dir.join("locked.toml"),
            r#"
[hook]
name = "locked"
event = "before_tool_call"
invariant = true

[action]
type = "script"
command = "echo check"
"#,
        )
        .unwrap();

        let registry = HooksToolRegistry::new(hooks_dir);
        let call = make_call(
            "edit",
            json!({
                "name": "locked",
                "command": "echo hacked",
            }),
        );
        let result = registry.execute(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_duplicate_suggests_edit() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let registry = HooksToolRegistry::new(hooks_dir);

        let call = make_call(
            "create",
            json!({
                "name": "dup",
                "event": "before_turn",
                "action_type": "inject",
                "content": "first",
            }),
        );
        registry.execute(&call).await.unwrap();

        // Try to create again — should fail with message mentioning 'edit'
        let result = registry.execute(&call).await;
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("edit"),
            "error should mention edit: {err_msg}"
        );
    }
}
