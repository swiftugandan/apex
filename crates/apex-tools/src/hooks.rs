use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use apex_core::domain::{
    HookActionType, HookDef, HookEvent, HookFilter, HookOutcome, OnFailureBehavior,
};
use apex_core::ports::HookRegistry;

/// Default cache TTL for hook reloading (5 seconds).
const HOOK_CACHE_TTL_SECS: u64 = 5;

/// Filesystem-based hook registry. Loads hook definitions from `.apex/hooks/<event>.d/*.toml`.
/// Uses interior mutability so hooks are reloaded from disk periodically,
/// ensuring newly-created hooks (via `manage_hooks`) are visible after a short delay.
/// Reloads are throttled to at most once per `HOOK_CACHE_TTL_SECS` to avoid
/// directory enumeration + TOML parsing on every dispatch.
pub struct FsHookRegistry {
    hooks_dir: PathBuf,
    hooks: RwLock<Vec<HookDef>>,
    last_loaded: Mutex<std::time::Instant>,
    cache_ttl_secs: u64,
}

impl FsHookRegistry {
    /// Create a new registry, loading hooks from the given directory.
    pub fn new(hooks_dir: PathBuf) -> Self {
        let hooks = Self::load_hooks_from_dir(&hooks_dir);
        Self {
            hooks_dir,
            hooks: RwLock::new(hooks),
            last_loaded: Mutex::new(std::time::Instant::now()),
            cache_ttl_secs: HOOK_CACHE_TTL_SECS,
        }
    }

    /// Create an empty registry (no hooks directory).
    pub fn empty() -> Self {
        Self {
            hooks_dir: PathBuf::new(),
            hooks: RwLock::new(Vec::new()),
            last_loaded: Mutex::new(std::time::Instant::now()),
            cache_ttl_secs: HOOK_CACHE_TTL_SECS,
        }
    }

    /// Create a registry with a custom cache TTL (for testing).
    #[cfg(test)]
    fn with_ttl(hooks_dir: PathBuf, ttl_secs: u64) -> Self {
        let hooks = Self::load_hooks_from_dir(&hooks_dir);
        Self {
            hooks_dir,
            hooks: RwLock::new(hooks),
            last_loaded: Mutex::new(std::time::Instant::now()),
            cache_ttl_secs: ttl_secs,
        }
    }

    /// Load all hooks from the given directory (non-async, used at construction time).
    fn load_hooks_from_dir(hooks_dir: &Path) -> Vec<HookDef> {
        let mut hooks = Vec::new();

        if !hooks_dir.exists() {
            return hooks;
        }

        let event_dirs = [
            "before_turn.d",
            "after_turn.d",
            "before_tool_call.d",
            "after_tool_result.d",
            "before_push.d",
            "after_claim.d",
            "on_success.d",
            "on_failure.d",
            "on_log.d",
        ];

        for dir_name in &event_dirs {
            let dir_path = hooks_dir.join(dir_name);
            if !dir_path.exists() {
                continue;
            }

            let entries = match std::fs::read_dir(&dir_path) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("warning: failed to read {}: {e}", dir_path.display());
                    continue;
                }
            };

            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("warning: failed to read entry: {e}");
                        continue;
                    }
                };
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    match Self::parse_hook(&path) {
                        Ok(hook) => hooks.push(hook),
                        Err(e) => eprintln!("warning: {e}"),
                    }
                }
            }
        }

        // Also scan top-level .toml files in hooks_dir
        if let Ok(entries) = std::fs::read_dir(hooks_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    match Self::parse_hook(&path) {
                        Ok(hook) => hooks.push(hook),
                        Err(e) => eprintln!("warning: {e}"),
                    }
                }
            }
        }

        hooks.sort_by_key(|h| h.hook.priority);
        hooks
    }

    /// Parse a single hook TOML file.
    fn parse_hook(path: &Path) -> Result<HookDef, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let mut hook: HookDef = toml::from_str(&content)
            .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
        hook.source_path = Some(path.to_string_lossy().to_string());
        Ok(hook)
    }

    /// Validate a hook definition.
    pub fn validate_hook(hook: &HookDef) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if hook.hook.name.is_empty() {
            errors.push("hook.name is required".to_string());
        }

        match hook.action.action_type {
            HookActionType::Script | HookActionType::Transform => {
                if hook.action.command.is_none() {
                    errors
                        .push("action.command is required for script/transform hooks".to_string());
                }
            }
            HookActionType::Inject => {
                if hook.action.content.is_none() && hook.action.command.is_none() {
                    errors.push(
                        "action.content or action.command is required for inject hooks".to_string(),
                    );
                }
            }
            HookActionType::Block => {
                if hook.action.message.is_none() {
                    errors.push("action.message is required for block hooks".to_string());
                }
            }
        }

        if hook.action.timeout_ms == 0 {
            errors.push("action.timeout_ms must be > 0".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Validate all hooks in the hooks directory.
    pub fn validate_all(&self) -> Vec<(String, Vec<String>)> {
        let hooks = self.hooks.read();
        let mut issues = Vec::new();
        for hook in hooks.iter() {
            if let Err(errs) = Self::validate_hook(hook) {
                let name = hook.source_path.as_deref().unwrap_or(&hook.hook.name);
                issues.push((name.to_string(), errs));
            }
        }
        issues
    }
}

/// Check if a hook should fire given the context and filter.
fn matches_filter(filter: &HookFilter, context: &Value) -> bool {
    if let Some(ref tool_name) = filter.tool {
        // Check if context has a "tool" or "name" field matching the filter
        let ctx_tool = context
            .get("tool")
            .or_else(|| context.get("name"))
            .and_then(Value::as_str);
        match ctx_tool {
            Some(name) => name == tool_name,
            None => false,
        }
    } else {
        true // No filter = always match
    }
}

/// Execute a script command with the given input, returning stdout.
async fn run_script(
    command: &str,
    input: Option<&str>,
    timeout_ms: u64,
    working_dir: Option<&Path>,
) -> Result<String, String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;

    // Write input to stdin
    if let Some(input_data) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let data = input_data.to_string();
            tokio::spawn(async move {
                let _ = stdin.write_all(data.as_bytes()).await;
                let _ = stdin.shutdown().await;
            });
        }
    } else {
        drop(child.stdin.take());
    }

    let timeout = Duration::from_millis(timeout_ms);
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("hook script timed out after {timeout_ms}ms"))?
        .map_err(|e| format!("failed to wait for script: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        return Err(format!("script exited with code {code}: {stderr}"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Execute a sequence of hooks, filtering and dispatching each in priority order.
/// Stops on the first Block outcome.
async fn dispatch_hooks(
    hooks: &[HookDef],
    context: &Value,
    working_dir: Option<&Path>,
) -> Vec<HookOutcome> {
    let mut outcomes = Vec::new();
    for hook in hooks {
        if !matches_filter(&hook.hook.filter, context) {
            continue;
        }
        let outcome = execute_hook(hook, context, working_dir).await;
        if matches!(&outcome, HookOutcome::Block(_)) {
            outcomes.push(outcome);
            break;
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// Execute a single hook action and return the outcome.
async fn execute_hook(hook: &HookDef, context: &Value, working_dir: Option<&Path>) -> HookOutcome {
    match hook.action.action_type {
        HookActionType::Block => {
            let msg = hook
                .action
                .message
                .clone()
                .unwrap_or_else(|| format!("Blocked by hook '{}'", hook.hook.name));
            HookOutcome::Block(msg)
        }
        HookActionType::Inject => {
            // If there's a command, run it and use output as inject content
            if let Some(ref cmd) = hook.action.command {
                let input_data = hook
                    .action
                    .input
                    .as_ref()
                    .map(|_| serde_json::to_string(context).unwrap_or_default());
                match run_script(
                    cmd,
                    input_data.as_deref(),
                    hook.action.timeout_ms,
                    working_dir,
                )
                .await
                {
                    Ok(output) => HookOutcome::Inject(output),
                    Err(e) => match hook.action.on_failure {
                        OnFailureBehavior::Block => HookOutcome::Block(format!(
                            "Inject hook '{}' failed: {e}",
                            hook.hook.name
                        )),
                        _ => {
                            eprintln!("warning: inject hook '{}' failed: {e}", hook.hook.name);
                            HookOutcome::Continue(None)
                        }
                    },
                }
            } else if let Some(ref content) = hook.action.content {
                HookOutcome::Inject(content.clone())
            } else {
                HookOutcome::Continue(None)
            }
        }
        HookActionType::Script | HookActionType::Transform => {
            let Some(ref cmd) = hook.action.command else {
                eprintln!("warning: hook '{}' has no command", hook.hook.name);
                return HookOutcome::Continue(None);
            };

            let input_data = hook
                .action
                .input
                .as_ref()
                .map(|_| serde_json::to_string(context).unwrap_or_default());

            match run_script(
                cmd,
                input_data.as_deref(),
                hook.action.timeout_ms,
                working_dir,
            )
            .await
            {
                Ok(output) => {
                    if hook.action.action_type == HookActionType::Transform {
                        HookOutcome::Continue(Some(output))
                    } else {
                        HookOutcome::Continue(None)
                    }
                }
                Err(e) => match hook.action.on_failure {
                    OnFailureBehavior::Block => {
                        HookOutcome::Block(format!("Hook '{}' failed: {e}", hook.hook.name))
                    }
                    OnFailureBehavior::Warn => {
                        eprintln!("warning: hook '{}' failed: {e}", hook.hook.name);
                        HookOutcome::Continue(None)
                    }
                    OnFailureBehavior::Continue => HookOutcome::Continue(None),
                },
            }
        }
    }
}

#[async_trait]
impl HookRegistry for FsHookRegistry {
    fn hooks_for(&self, event: HookEvent) -> Vec<HookDef> {
        let hooks = self.hooks.read();
        let mut filtered: Vec<_> = hooks
            .iter()
            .filter(|h| h.hook.event == event)
            .cloned()
            .collect();
        filtered.sort_by_key(|h| h.hook.priority);
        filtered
    }

    fn all_hooks(&self) -> Vec<HookDef> {
        self.hooks.read().clone()
    }

    async fn dispatch(&self, event: HookEvent, context: &Value) -> Vec<HookOutcome> {
        // Reload hooks from disk if the cache TTL has elapsed. Use spawn_blocking
        // so sync filesystem I/O does not block the async executor.
        {
            let elapsed = self.last_loaded.lock().elapsed();
            if elapsed >= std::time::Duration::from_secs(self.cache_ttl_secs) {
                let hooks_dir = self.hooks_dir.clone();
                if let Ok(fresh) =
                    tokio::task::spawn_blocking(move || Self::load_hooks_from_dir(&hooks_dir)).await
                {
                    let mut guard = self.hooks.write();
                    *guard = fresh;
                    drop(guard);
                    *self.last_loaded.lock() = std::time::Instant::now();
                }
                // If spawn_blocking join failed, keep existing hooks and continue
            }
        }

        let hooks = {
            let guard = self.hooks.read();
            let mut filtered: Vec<_> = guard
                .iter()
                .filter(|h| h.hook.event == event)
                .cloned()
                .collect();
            filtered.sort_by_key(|h| h.hook.priority);
            filtered
        };

        // Derive working directory from hooks_dir parent (the .apex directory's parent = project root)
        let working_dir = self.hooks_dir.parent().and_then(|apex| apex.parent());

        dispatch_hooks(&hooks, context, working_dir).await
    }

    fn reload(&mut self) -> Result<(), String> {
        let fresh = Self::load_hooks_from_dir(&self.hooks_dir);
        *self.hooks.get_mut() = fresh;
        *self.last_loaded.get_mut() = std::time::Instant::now();
        Ok(())
    }

    fn has_hooks_for(&self, event: HookEvent) -> bool {
        let hooks = self.hooks.read();
        hooks.iter().any(|h| h.hook.event == event)
    }
}

/// In-memory hook registry holding only propagatable hooks from a parent registry.
/// Used by sub-agents to inherit a filtered subset of the parent's hooks without disk I/O.
pub struct FilteredHookRegistry {
    hooks: Vec<HookDef>,
}

impl FilteredHookRegistry {
    /// Build a filtered registry containing only hooks with `propagate: true`.
    /// Returns `None` if no hooks qualify (so the sub-agent can skip hook dispatch entirely).
    pub fn from_propagatable(parent: &dyn HookRegistry) -> Option<Arc<dyn HookRegistry>> {
        let hooks: Vec<_> = parent
            .all_hooks()
            .into_iter()
            .filter(|h| h.hook.propagate)
            .collect();
        if hooks.is_empty() {
            None
        } else {
            Some(Arc::new(Self { hooks }))
        }
    }
}

#[async_trait]
impl HookRegistry for FilteredHookRegistry {
    fn hooks_for(&self, event: HookEvent) -> Vec<HookDef> {
        let mut filtered: Vec<_> = self
            .hooks
            .iter()
            .filter(|h| h.hook.event == event)
            .cloned()
            .collect();
        filtered.sort_by_key(|h| h.hook.priority);
        filtered
    }

    fn all_hooks(&self) -> Vec<HookDef> {
        self.hooks.clone()
    }

    async fn dispatch(&self, event: HookEvent, context: &Value) -> Vec<HookOutcome> {
        let hooks = self.hooks_for(event);
        dispatch_hooks(&hooks, context, None).await
    }

    fn reload(&mut self) -> Result<(), String> {
        Ok(()) // No-op: in-memory hooks don't reload from disk
    }

    fn has_hooks_for(&self, event: HookEvent) -> bool {
        self.hooks.iter().any(|h| h.hook.event == event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::{HookAction, HookMeta};
    use tempfile::TempDir;

    #[test]
    fn parse_hook_from_toml() {
        let dir = TempDir::new().unwrap();
        let hook_path = dir.path().join("test_hook.toml");
        std::fs::write(
            &hook_path,
            r#"
[hook]
name = "test_hook"
event = "before_tool_call"
priority = 10

[hook.filter]
tool = "shell_exec"

[action]
type = "script"
command = "echo ok"
input = "tool_call"
timeout_ms = 3000
on_failure = "block"
"#,
        )
        .unwrap();

        let hook = FsHookRegistry::parse_hook(&hook_path).unwrap();
        assert_eq!(hook.hook.name, "test_hook");
        assert_eq!(hook.hook.event, HookEvent::BeforeToolCall);
        assert_eq!(hook.hook.priority, 10);
        assert_eq!(hook.hook.filter.tool.as_deref(), Some("shell_exec"));
        assert_eq!(hook.action.action_type, HookActionType::Script);
        assert_eq!(hook.action.command.as_deref(), Some("echo ok"));
        assert_eq!(hook.action.timeout_ms, 3000);
    }

    #[test]
    fn empty_registry_returns_no_hooks() {
        let registry = FsHookRegistry::empty();
        assert!(registry.hooks_for(HookEvent::BeforeTurn).is_empty());
        assert!(registry.all_hooks().is_empty());
    }

    #[test]
    fn load_hooks_from_event_directories() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let event_dir = hooks_dir.join("before_turn.d");
        std::fs::create_dir_all(&event_dir).unwrap();

        std::fs::write(
            event_dir.join("inject_context.toml"),
            r#"
[hook]
name = "inject_context"
event = "before_turn"
priority = 5

[action]
type = "inject"
content = "Remember: always be careful with file operations."
"#,
        )
        .unwrap();

        let registry = FsHookRegistry::new(hooks_dir);
        let hooks = registry.hooks_for(HookEvent::BeforeTurn);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].hook.name, "inject_context");
    }

    #[test]
    fn hooks_sorted_by_priority() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let event_dir = hooks_dir.join("before_tool_call.d");
        std::fs::create_dir_all(&event_dir).unwrap();

        std::fs::write(
            event_dir.join("low_priority.toml"),
            r#"
[hook]
name = "low"
event = "before_tool_call"
priority = 100

[action]
type = "script"
command = "echo low"
"#,
        )
        .unwrap();

        std::fs::write(
            event_dir.join("high_priority.toml"),
            r#"
[hook]
name = "high"
event = "before_tool_call"
priority = 1

[action]
type = "script"
command = "echo high"
"#,
        )
        .unwrap();

        let registry = FsHookRegistry::new(hooks_dir);
        let hooks = registry.hooks_for(HookEvent::BeforeToolCall);
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].hook.name, "high");
        assert_eq!(hooks[1].hook.name, "low");
    }

    #[test]
    fn filter_matches_tool_name() {
        let filter = HookFilter {
            tool: Some("shell_exec".to_string()),
        };
        let ctx = serde_json::json!({"tool": "shell_exec", "input": {}});
        assert!(matches_filter(&filter, &ctx));

        let ctx2 = serde_json::json!({"tool": "file_read", "input": {}});
        assert!(!matches_filter(&filter, &ctx2));
    }

    #[test]
    fn filter_no_filter_matches_all() {
        let filter = HookFilter::default();
        let ctx = serde_json::json!({"tool": "anything"});
        assert!(matches_filter(&filter, &ctx));
    }

    #[test]
    fn validate_hook_requires_name() {
        let hook = HookDef {
            hook: HookMeta {
                name: String::new(),
                event: HookEvent::BeforeTurn,
                filter: HookFilter::default(),
                priority: 50,
                invariant: false,
                propagate: false,
            },
            action: HookAction {
                action_type: HookActionType::Script,
                command: Some("echo ok".into()),
                input: None,
                timeout_ms: 5000,
                content: None,
                on_failure: OnFailureBehavior::Warn,
                message: None,
            },
            source_path: None,
        };
        assert!(FsHookRegistry::validate_hook(&hook).is_err());
    }

    #[test]
    fn validate_script_hook_requires_command() {
        let hook = HookDef {
            hook: HookMeta {
                name: "test".into(),
                event: HookEvent::BeforeTurn,
                filter: HookFilter::default(),
                priority: 50,
                invariant: false,
                propagate: false,
            },
            action: HookAction {
                action_type: HookActionType::Script,
                command: None,
                input: None,
                timeout_ms: 5000,
                content: None,
                on_failure: OnFailureBehavior::Warn,
                message: None,
            },
            source_path: None,
        };
        assert!(FsHookRegistry::validate_hook(&hook).is_err());
    }

    #[tokio::test]
    async fn dispatch_script_hook() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let event_dir = hooks_dir.join("before_turn.d");
        std::fs::create_dir_all(&event_dir).unwrap();

        std::fs::write(
            event_dir.join("test.toml"),
            r#"
[hook]
name = "test_script"
event = "before_turn"

[action]
type = "script"
command = "echo hello"
"#,
        )
        .unwrap();

        let registry = FsHookRegistry::new(hooks_dir);
        let outcomes = registry
            .dispatch(HookEvent::BeforeTurn, &serde_json::json!({}))
            .await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], HookOutcome::Continue(None)));
    }

    #[tokio::test]
    async fn dispatch_block_hook() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let event_dir = hooks_dir.join("before_tool_call.d");
        std::fs::create_dir_all(&event_dir).unwrap();

        std::fs::write(
            event_dir.join("block_rm.toml"),
            r#"
[hook]
name = "block_rm"
event = "before_tool_call"

[hook.filter]
tool = "shell_exec"

[action]
type = "block"
message = "Command blocked by safety hook"
"#,
        )
        .unwrap();

        let registry = FsHookRegistry::new(hooks_dir);
        let outcomes = registry
            .dispatch(
                HookEvent::BeforeToolCall,
                &serde_json::json!({"tool": "shell_exec"}),
            )
            .await;
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            HookOutcome::Block(msg) => assert_eq!(msg, "Command blocked by safety hook"),
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn dispatch_inject_hook() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let event_dir = hooks_dir.join("before_turn.d");
        std::fs::create_dir_all(&event_dir).unwrap();

        std::fs::write(
            event_dir.join("inject.toml"),
            r#"
[hook]
name = "inject_facts"
event = "before_turn"

[action]
type = "inject"
content = "Remember: this project uses Rust."
"#,
        )
        .unwrap();

        let registry = FsHookRegistry::new(hooks_dir);
        let outcomes = registry
            .dispatch(HookEvent::BeforeTurn, &serde_json::json!({}))
            .await;
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            HookOutcome::Inject(content) => {
                assert_eq!(content, "Remember: this project uses Rust.")
            }
            other => panic!("expected Inject, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn dispatch_skips_filtered_hooks() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        let event_dir = hooks_dir.join("before_tool_call.d");
        std::fs::create_dir_all(&event_dir).unwrap();

        std::fs::write(
            event_dir.join("shell_only.toml"),
            r#"
[hook]
name = "shell_only"
event = "before_tool_call"

[hook.filter]
tool = "shell_exec"

[action]
type = "script"
command = "echo ok"
"#,
        )
        .unwrap();

        let registry = FsHookRegistry::new(hooks_dir);

        // Should not fire for file_read
        let outcomes = registry
            .dispatch(
                HookEvent::BeforeToolCall,
                &serde_json::json!({"tool": "file_read"}),
            )
            .await;
        assert!(outcomes.is_empty());

        // Should fire for shell_exec
        let outcomes = registry
            .dispatch(
                HookEvent::BeforeToolCall,
                &serde_json::json!({"tool": "shell_exec"}),
            )
            .await;
        assert_eq!(outcomes.len(), 1);
    }

    #[tokio::test]
    async fn dispatch_sees_newly_created_hooks() {
        let dir = TempDir::new().unwrap();
        let hooks_dir = dir.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();

        // Create registry with TTL=0 so reload happens on every dispatch
        let registry = FsHookRegistry::with_ttl(hooks_dir.clone(), 0);
        let outcomes = registry
            .dispatch(HookEvent::BeforeTurn, &serde_json::json!({}))
            .await;
        assert!(outcomes.is_empty());

        // Now create a hook on disk (simulating manage_hooks create)
        let event_dir = hooks_dir.join("before_turn.d");
        std::fs::create_dir_all(&event_dir).unwrap();
        std::fs::write(
            event_dir.join("late_hook.toml"),
            r#"
[hook]
name = "late_hook"
event = "before_turn"

[action]
type = "inject"
content = "I was created after init."
"#,
        )
        .unwrap();

        // dispatch should reload and see the new hook
        let outcomes = registry
            .dispatch(HookEvent::BeforeTurn, &serde_json::json!({}))
            .await;
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            HookOutcome::Inject(content) => {
                assert_eq!(content, "I was created after init.");
            }
            other => panic!("expected Inject, got {:?}", other),
        }
    }
}
