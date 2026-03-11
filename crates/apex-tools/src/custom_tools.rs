use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use apex_core::domain::{Skill, SkillId, SkillStatus, ToolCall, ToolDef, ToolResult, ToolSchema};
use apex_core::error::ToolError;
use apex_core::ports::{SkillStore, ToolRegistry};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

use crate::spill::{
    SpillManager, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_SPILL_HEAD_LINES, DEFAULT_SPILL_STRATEGY,
    DEFAULT_SPILL_TAIL_LINES,
};

// ── Manifest types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolManifest {
    #[serde(default)]
    tool: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub name: String,
    pub description: String,
    pub created_at: String,
    pub script: String,
    pub schema_file: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    pub task_pattern: Option<String>,
}

fn default_timeout() -> u64 {
    30
}

// Builtin tool names that cannot be overridden.
const BUILTIN_NAMES: &[&str] = &[
    "shell_exec",
    "file_read",
    "file_write",
    "file_edit",
    "glob",
    "grep",
    "create_tool",
    "scratchpad_load",
    "scratchpad_save",
    "scratchpad_update_subtask",
    "scratchpad_add_note",
    "memory_store_fact",
    "memory_query_facts",
    "memory_store_skill",
    "memory_find_skill",
    "memory_store_strategy",
    "memory_find_strategy",
    "queue_create_subtask",
    "queue_read_done",
    "queue_depth",
    "update_config",
];

// ── Registry ──────────────────────────────────────────────────────

pub struct CustomToolRegistry {
    tools_dir: PathBuf,
    entries: RwLock<Vec<ManifestEntry>>,
    spill: SpillManager,
    skill_store: Option<Arc<dyn SkillStore>>,
}

impl CustomToolRegistry {
    /// Create a new registry rooted at `tools_dir` (the `tools/` directory).
    /// Loads existing manifest.toml if present.
    pub fn new(
        tools_dir: PathBuf,
        spill: SpillManager,
        skill_store: Option<Arc<dyn SkillStore>>,
    ) -> Self {
        let entries = Self::load_manifest(&tools_dir).unwrap_or_default();
        Self {
            tools_dir,
            entries: RwLock::new(entries),
            spill,
            skill_store,
        }
    }

    fn manifest_path(tools_dir: &Path) -> PathBuf {
        tools_dir.join("manifest.toml")
    }

    fn custom_dir(tools_dir: &Path) -> PathBuf {
        tools_dir.join("custom")
    }

    fn load_manifest(tools_dir: &Path) -> Option<Vec<ManifestEntry>> {
        let path = Self::manifest_path(tools_dir);
        let content = std::fs::read_to_string(&path).ok()?;
        let manifest: ToolManifest = toml::from_str(&content).ok()?;
        Some(manifest.tool)
    }

    fn save_manifest(tools_dir: &Path, entries: &[ManifestEntry]) -> Result<(), String> {
        let manifest = ToolManifest {
            tool: entries.to_vec(),
        };
        let content =
            toml::to_string_pretty(&manifest).map_err(|e| format!("TOML serialize: {e}"))?;
        let path = Self::manifest_path(tools_dir);
        std::fs::write(&path, content).map_err(|e| format!("write manifest: {e}"))?;
        Ok(())
    }
}

// ── create_tool definition ────────────────────────────────────────

fn create_tool_definition() -> ToolDef {
    ToolDef {
        schema: ToolSchema {
            name: "create_tool".into(),
            description: "Create a new custom tool. Writes implementation script and schema, \
                          runs tests, and registers the tool for immediate use."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Tool name (alphanumeric and hyphens only, e.g. 'csv-parser')"
                    },
                    "description": {
                        "type": "string",
                        "description": "What the tool does (shown to LLM)"
                    },
                    "implementation": {
                        "type": "string",
                        "description": "Shell script content for run.sh. Receives JSON input on stdin, must write JSON to stdout."
                    },
                    "input_schema": {
                        "type": "object",
                        "description": "JSON Schema for the tool's input (must have type: object)"
                    },
                    "test_script": {
                        "type": "string",
                        "description": "Shell script to test the tool. Exit 0 = pass, non-zero = fail."
                    },
                    "task_pattern": {
                        "type": "string",
                        "description": "Optional: regex/pattern describing tasks this tool helps with (stored as a skill)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Max execution time in seconds (default 30)"
                    }
                },
                "required": ["name", "description", "implementation", "input_schema", "test_script"]
            }),
        },
    }
}

// ── ToolRegistry impl ─────────────────────────────────────────────

#[async_trait]
impl ToolRegistry for CustomToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        let mut defs = vec![create_tool_definition()];

        // We need a blocking read here since definitions() is sync.
        // Use try_read to avoid deadlocks; if locked, return just create_tool.
        if let Ok(entries) = self.entries.try_read() {
            for entry in entries.iter() {
                let schema_path = Self::custom_dir(&self.tools_dir)
                    .join(&entry.name)
                    .join(&entry.schema_file);
                if let Ok(schema_content) = std::fs::read_to_string(&schema_path) {
                    if let Ok(input_schema) = serde_json::from_str(&schema_content) {
                        defs.push(ToolDef {
                            schema: ToolSchema {
                                name: entry.name.clone(),
                                description: entry.description.clone(),
                                input_schema,
                            },
                        });
                    }
                }
            }
        }

        defs
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if call.name == "create_tool" {
            return self.handle_create_tool(call).await;
        }

        // Check if it's a registered custom tool
        let entries = self.entries.read().await;
        if entries.iter().any(|e| e.name == call.name) {
            drop(entries);
            return self.handle_custom_exec(call).await;
        }

        Err(ToolError::UnknownTool(call.name.clone()))
    }
}

// ── Validation helpers ────────────────────────────────────────────

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("tool name cannot be empty".into());
    }
    if name.len() > 64 {
        return Err("tool name too long (max 64 chars)".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("tool name must contain only alphanumeric characters and hyphens".into());
    }
    if BUILTIN_NAMES.contains(&name) {
        return Err(format!("'{name}' conflicts with a builtin tool name"));
    }
    Ok(())
}

fn validate_schema(schema: &serde_json::Value) -> Result<(), String> {
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("object") => Ok(()),
        _ => Err("input_schema must have \"type\": \"object\"".into()),
    }
}

// ── Handlers ──────────────────────────────────────────────────────

impl CustomToolRegistry {
    async fn handle_create_tool(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let start = Instant::now();

        let name = call.input["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'name'".into()))?;
        let description = call.input["description"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'description'".into()))?;
        let implementation = call.input["implementation"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'implementation'".into()))?;
        let input_schema = &call.input["input_schema"];
        let test_script = call.input["test_script"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing 'test_script'".into()))?;
        let task_pattern = call.input["task_pattern"].as_str().map(|s| s.to_string());
        let timeout_secs = call.input["timeout_secs"].as_u64().unwrap_or(30);

        // Validate
        if let Err(e) = validate_name(name) {
            return Ok(error_result(call, &e, start));
        }
        if description.is_empty() {
            return Ok(error_result(call, "description cannot be empty", start));
        }
        if let Err(e) = validate_schema(input_schema) {
            return Ok(error_result(call, &e, start));
        }

        // Check for duplicate
        {
            let entries = self.entries.read().await;
            if entries.iter().any(|e| e.name == name) {
                return Ok(error_result(
                    call,
                    &format!("tool '{name}' already exists"),
                    start,
                ));
            }
        }

        // Create directory
        let tool_dir = Self::custom_dir(&self.tools_dir).join(name);
        if let Err(e) = std::fs::create_dir_all(&tool_dir) {
            return Ok(error_result(
                call,
                &format!("failed to create directory: {e}"),
                start,
            ));
        }

        // Write run.sh
        let run_path = tool_dir.join("run.sh");
        if let Err(e) = std::fs::write(&run_path, implementation) {
            let _ = std::fs::remove_dir_all(&tool_dir);
            return Ok(error_result(call, &format!("write run.sh: {e}"), start));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&run_path, std::fs::Permissions::from_mode(0o755));
        }

        // Write schema.json
        let schema_path = tool_dir.join("schema.json");
        let schema_str = serde_json::to_string_pretty(input_schema).unwrap_or_default();
        if let Err(e) = std::fs::write(&schema_path, &schema_str) {
            let _ = std::fs::remove_dir_all(&tool_dir);
            return Ok(error_result(
                call,
                &format!("write schema.json: {e}"),
                start,
            ));
        }

        // Write test.sh
        let test_path = tool_dir.join("test.sh");
        if let Err(e) = std::fs::write(&test_path, test_script) {
            let _ = std::fs::remove_dir_all(&tool_dir);
            return Ok(error_result(call, &format!("write test.sh: {e}"), start));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&test_path, std::fs::Permissions::from_mode(0o755));
        }

        // Run tests with timeout
        let test_result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(test_script)
                .current_dir(&tool_dir)
                .output(),
        )
        .await;

        match test_result {
            Ok(Ok(output)) if output.status.success() => {
                // Tests passed — register
            }
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let _ = std::fs::remove_dir_all(&tool_dir);
                return Ok(error_result(
                    call,
                    &format!(
                        "tests failed (exit {})\nstdout: {}\nstderr: {}",
                        output.status.code().unwrap_or(-1),
                        stdout,
                        stderr
                    ),
                    start,
                ));
            }
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&tool_dir);
                return Ok(error_result(
                    call,
                    &format!("failed to run tests: {e}"),
                    start,
                ));
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&tool_dir);
                return Ok(error_result(
                    call,
                    &format!("tests timed out after {timeout_secs}s"),
                    start,
                ));
            }
        }

        // Build manifest entry
        let entry = ManifestEntry {
            name: name.to_string(),
            description: description.to_string(),
            created_at: now_iso(),
            script: "run.sh".to_string(),
            schema_file: "schema.json".to_string(),
            timeout_secs,
            task_pattern: task_pattern.clone(),
        };

        // Update manifest (read-modify-write)
        {
            let mut entries = self.entries.write().await;
            entries.push(entry);
            if let Err(e) = Self::save_manifest(&self.tools_dir, &entries) {
                eprintln!("warning: failed to save manifest: {e}");
            }
        }

        // Store skill if task_pattern provided
        if let Some(ref pattern) = task_pattern {
            if let Some(ref skill_store) = self.skill_store {
                let skill = Skill {
                    id: SkillId(format!("skill-tool-{name}")),
                    name: apex_core::domain::slugify(name),
                    description: description.to_string(),
                    task_pattern: pattern.clone(),
                    approach: format!("Use the custom tool '{name}': {description}"),
                    tools_used: vec![name.to_string()],
                    criteria_template: None,
                    success_count: 0,
                    failure_count: 0,
                    fitness: 0.5,
                    min_samples: 3,
                    last_used: now_iso(),
                    notes: format!("Auto-created when tool '{name}' was registered"),
                    status: SkillStatus::Active,
                };
                let _ = skill_store.store_skill(skill).await;
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({
                "status": "created",
                "tool_name": name,
                "path": tool_dir.to_string_lossy(),
                "message": format!("Tool '{name}' created and ready for use")
            }),
            is_error: false,
            duration_ms,
            ..Default::default()
        })
    }

    async fn handle_custom_exec(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let start = Instant::now();

        let entry = {
            let entries = self.entries.read().await;
            entries
                .iter()
                .find(|e| e.name == call.name)
                .cloned()
                .ok_or_else(|| ToolError::UnknownTool(call.name.clone()))?
        };

        let script_path = Self::custom_dir(&self.tools_dir)
            .join(&entry.name)
            .join(&entry.script);

        if !script_path.exists() {
            return Ok(error_result(
                call,
                &format!("script not found: {}", script_path.display()),
                start,
            ));
        }

        let input_json = serde_json::to_string(&call.input).unwrap_or_else(|_| "{}".into());

        // Run script with input piped to stdin
        let mut child = tokio::process::Command::new("sh")
            .arg(&script_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(Self::custom_dir(&self.tools_dir).join(&entry.name))
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn: {e}")))?;

        // Write input to stdin
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input_json.as_bytes()).await;
            drop(stdin);
        }

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(entry.timeout_secs),
            child.wait_with_output(),
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let is_error = !output.status.success();

                // Try to parse stdout as JSON, otherwise wrap it
                let parsed_output: serde_json::Value = serde_json::from_str(&stdout)
                    .unwrap_or_else(
                        |_| json!({ "output": stdout.trim(), "stderr": stderr.trim() }),
                    );

                // Apply spill if output is large
                let output_str = serde_json::to_string(&parsed_output).unwrap_or_default();
                let max_output: usize = DEFAULT_MAX_OUTPUT_BYTES;
                let (final_output, spill_path, stats, truncated) = if output_str.len() > max_output
                {
                    if let Some(spill_result) = self.spill.spill_if_needed(
                        &output_str,
                        max_output,
                        DEFAULT_SPILL_STRATEGY,
                        DEFAULT_SPILL_HEAD_LINES,
                        DEFAULT_SPILL_TAIL_LINES,
                    ) {
                        (
                            json!({ "output": spill_result.envelope }),
                            Some(spill_result.path),
                            Some(spill_result.stats),
                            true,
                        )
                    } else {
                        (parsed_output, None, None, false)
                    }
                } else {
                    (parsed_output, None, None, false)
                };

                Ok(ToolResult {
                    tool_use_id: call.id.clone(),
                    name: call.name.clone(),
                    output: final_output,
                    is_error,
                    spill_path,
                    stats,
                    truncated,
                    duration_ms,
                })
            }
            Ok(Err(e)) => Ok(error_result(call, &format!("execution failed: {e}"), start)),
            Err(_) => Ok(error_result(
                call,
                &format!("timed out after {}s", entry.timeout_secs),
                start,
            )),
        }
    }
}

use crate::tool_result_helpers::error_result;

fn now_iso() -> String {
    apex_core::now_unix_ts()
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_tools_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let tools_dir = dir.path().join("tools");
        std::fs::create_dir_all(tools_dir.join("custom")).unwrap();
        (dir, tools_dir)
    }

    fn make_registry(tools_dir: PathBuf) -> CustomToolRegistry {
        let spill = SpillManager::new(tools_dir.join("scratch"));
        CustomToolRegistry::new(tools_dir, spill, None)
    }

    fn make_call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "test-id".into(),
            name: name.into(),
            input,
        }
    }

    // ── Manifest parsing ──────────────────────────────────────────

    #[test]
    fn parse_empty_manifest() {
        let manifest: ToolManifest = toml::from_str("").unwrap();
        assert!(manifest.tool.is_empty());
    }

    #[test]
    fn parse_manifest_with_entries() {
        let toml_str = r#"
[[tool]]
name = "csv-parser"
description = "Parse CSV files"
created_at = "12345"
script = "run.sh"
schema_file = "schema.json"
timeout_secs = 30

[[tool]]
name = "json-validator"
description = "Validate JSON"
created_at = "12346"
script = "run.sh"
schema_file = "schema.json"
timeout_secs = 10
task_pattern = "validate.*json"
"#;
        let manifest: ToolManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(manifest.tool.len(), 2);
        assert_eq!(manifest.tool[0].name, "csv-parser");
        assert_eq!(
            manifest.tool[1].task_pattern.as_deref(),
            Some("validate.*json")
        );
    }

    #[test]
    fn manifest_roundtrip() {
        let entries = vec![ManifestEntry {
            name: "my-tool".into(),
            description: "A test tool".into(),
            created_at: "12345".into(),
            script: "run.sh".into(),
            schema_file: "schema.json".into(),
            timeout_secs: 30,
            task_pattern: None,
        }];
        let manifest = ToolManifest {
            tool: entries.clone(),
        };
        let serialized = toml::to_string_pretty(&manifest).unwrap();
        let parsed: ToolManifest = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.tool.len(), 1);
        assert_eq!(parsed.tool[0].name, "my-tool");
    }

    // ── Name validation ───────────────────────────────────────────

    #[test]
    fn valid_names() {
        assert!(validate_name("csv-parser").is_ok());
        assert!(validate_name("tool1").is_ok());
        assert!(validate_name("a").is_ok());
    }

    #[test]
    fn reject_empty_name() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn reject_spaces() {
        assert!(validate_name("my tool").is_err());
    }

    #[test]
    fn reject_special_chars() {
        assert!(validate_name("tool_name").is_err());
        assert!(validate_name("tool.name").is_err());
    }

    #[test]
    fn reject_builtin_names() {
        assert!(validate_name("shell_exec").is_err());
        assert!(validate_name("file_read").is_err());
        assert!(validate_name("file_write").is_err());
        assert!(validate_name("file_edit").is_err());
        assert!(validate_name("glob").is_err());
        assert!(validate_name("grep").is_err());
        assert!(validate_name("create_tool").is_err());
    }

    // ── Schema validation ─────────────────────────────────────────

    #[test]
    fn valid_schema() {
        let schema = json!({"type": "object", "properties": {}});
        assert!(validate_schema(&schema).is_ok());
    }

    #[test]
    fn reject_non_object_schema() {
        let schema = json!({"type": "string"});
        assert!(validate_schema(&schema).is_err());
    }

    #[test]
    fn reject_missing_type() {
        let schema = json!({"properties": {}});
        assert!(validate_schema(&schema).is_err());
    }

    // ── File creation and layout ──────────────────────────────────

    #[tokio::test]
    async fn create_tool_writes_files() {
        let (_dir, tools_dir) = temp_tools_dir();
        let registry = make_registry(tools_dir.clone());

        let call = make_call(
            "create_tool",
            json!({
                "name": "echo-tool",
                "description": "Echoes input back",
                "implementation": "#!/bin/sh\ncat",
                "input_schema": {"type": "object", "properties": {"msg": {"type": "string"}}},
                "test_script": "#!/bin/sh\necho '{\"msg\":\"hi\"}' | sh run.sh | grep -q msg",
            }),
        );

        let result = registry.execute(&call).await.unwrap();
        assert!(!result.is_error, "expected success: {:?}", result.output);

        let tool_path = tools_dir.join("custom").join("echo-tool");
        assert!(tool_path.join("run.sh").exists());
        assert!(tool_path.join("schema.json").exists());
        assert!(tool_path.join("test.sh").exists());

        // Manifest should be written
        assert!(tools_dir.join("manifest.toml").exists());
        let manifest_content = std::fs::read_to_string(tools_dir.join("manifest.toml")).unwrap();
        assert!(manifest_content.contains("echo-tool"));
    }

    // ── Test execution (pass and fail) ────────────────────────────

    #[tokio::test]
    async fn create_tool_fails_when_tests_fail() {
        let (_dir, tools_dir) = temp_tools_dir();
        let registry = make_registry(tools_dir.clone());

        let call = make_call(
            "create_tool",
            json!({
                "name": "bad-tool",
                "description": "A tool that fails tests",
                "implementation": "#!/bin/sh\necho broken",
                "input_schema": {"type": "object"},
                "test_script": "#!/bin/sh\nexit 1",
            }),
        );

        let result = registry.execute(&call).await.unwrap();
        assert!(result.is_error);
        assert!(result.output["error"]
            .as_str()
            .unwrap()
            .contains("tests failed"));

        // Directory should be cleaned up
        assert!(!tools_dir.join("custom").join("bad-tool").exists());
    }

    // ── Custom tool execution ─────────────────────────────────────

    #[tokio::test]
    async fn execute_custom_tool_with_json_io() {
        let (_dir, tools_dir) = temp_tools_dir();
        let registry = make_registry(tools_dir.clone());

        // First create the tool
        let create_call = make_call(
            "create_tool",
            json!({
                "name": "upper-tool",
                "description": "Uppercases input",
                "implementation": "#!/bin/sh\nread input\necho \"{\\\"result\\\": \\\"HELLO\\\"}\"",
                "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}},
                "test_script": "#!/bin/sh\necho '{\"text\":\"hi\"}' | sh run.sh | grep -q result",
            }),
        );
        let result = registry.execute(&create_call).await.unwrap();
        assert!(!result.is_error, "create failed: {:?}", result.output);

        // Now execute it
        let exec_call = make_call("upper-tool", json!({"text": "hello"}));
        let result = registry.execute(&exec_call).await.unwrap();
        assert!(!result.is_error, "exec failed: {:?}", result.output);
        assert!(result.output.get("result").is_some() || result.output.get("output").is_some());
    }

    // ── definitions() includes create_tool + registered tools ─────

    #[tokio::test]
    async fn definitions_always_includes_create_tool() {
        let (_dir, tools_dir) = temp_tools_dir();
        let registry = make_registry(tools_dir);

        let defs = registry.definitions();
        assert!(defs.iter().any(|d| d.schema.name == "create_tool"));
    }

    #[tokio::test]
    async fn definitions_includes_registered_tools() {
        let (_dir, tools_dir) = temp_tools_dir();
        let registry = make_registry(tools_dir.clone());

        // Create a tool
        let call = make_call(
            "create_tool",
            json!({
                "name": "my-tool",
                "description": "Test tool",
                "implementation": "#!/bin/sh\ncat",
                "input_schema": {"type": "object", "properties": {}},
                "test_script": "#!/bin/sh\ntrue",
            }),
        );
        let result = registry.execute(&call).await.unwrap();
        assert!(!result.is_error, "{:?}", result.output);

        let defs = registry.definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.schema.name.as_str()).collect();
        assert!(names.contains(&"create_tool"));
        assert!(names.contains(&"my-tool"));
    }

    // ── Unknown tool returns error ────────────────────────────────

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let (_dir, tools_dir) = temp_tools_dir();
        let registry = make_registry(tools_dir);

        let call = make_call("nonexistent", json!({}));
        let err = registry.execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::UnknownTool(_)));
    }

    // ── Duplicate name rejected ───────────────────────────────────

    #[tokio::test]
    async fn reject_duplicate_name() {
        let (_dir, tools_dir) = temp_tools_dir();
        let registry = make_registry(tools_dir);

        let call = make_call(
            "create_tool",
            json!({
                "name": "dup-tool",
                "description": "First",
                "implementation": "#!/bin/sh\ncat",
                "input_schema": {"type": "object"},
                "test_script": "#!/bin/sh\ntrue",
            }),
        );
        let r1 = registry.execute(&call).await.unwrap();
        assert!(!r1.is_error);

        // Try to create again
        let r2 = registry.execute(&call).await.unwrap();
        assert!(r2.is_error);
        assert!(r2.output["error"]
            .as_str()
            .unwrap()
            .contains("already exists"));
    }
}
