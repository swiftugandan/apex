use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON schema for a tool, sent to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Controls whether a tool's full schema is sent eagerly or loaded on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolLoading {
    /// Full schema sent to the LLM every turn.
    #[default]
    Eager,
    /// Only name + description sent; full schema loaded via `load_tool_definitions`.
    Deferred,
}

/// A tool definition (schema + metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub schema: ToolSchema,
    #[serde(default)]
    pub loading: ToolLoading,
}

impl ToolDef {
    /// Create an eager tool definition (full schema sent every turn).
    pub fn eager(schema: ToolSchema) -> Self {
        Self {
            schema,
            loading: ToolLoading::Eager,
        }
    }
}

/// A tool invocation requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// Result of executing a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub name: String,
    #[serde(default)]
    pub output: Value,
    pub is_error: bool,
    #[serde(default)]
    pub spill_path: Option<String>,
    #[serde(default)]
    pub stats: Option<OutputStats>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub duration_ms: u64,
}

/// Optional breakdown of output token usage (e.g. reasoning vs content).
/// Populated when the provider returns details (e.g. OpenAI completion_tokens_details).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct OutputTokensDetails {
    /// Tokens used for reasoning/thinking (e.g. o1/o3, extended thinking).
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

/// Token usage from an LLM response.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    /// Optional breakdown (e.g. reasoning_tokens); set when provider supplies it.
    #[serde(default)]
    pub output_tokens_details: Option<OutputTokensDetails>,
}

/// Hint for providers about block cacheability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheHint {
    /// Block is stable across turns — providers may cache it.
    Static,
    /// Block changes between requests — should not be cached.
    Dynamic,
}

/// A block of the system prompt, with a cache hint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    pub cache_hint: CacheHint,
}

/// Content type for token estimation calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentType {
    Prose,
    Code,
    Mixed,
}

/// Calibration data for token estimation, updated from actual LLM responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationData {
    pub chars_per_token_prose: f32,
    pub chars_per_token_code: f32,
    pub chars_per_token_mixed: f32,
    pub sample_count: u32,
    /// EMA of reasoning tokens per turn (when output_tokens_details is present).
    #[serde(default)]
    pub reasoning_tokens_ema: Option<f32>,
    /// Number of samples used for reasoning EMA.
    #[serde(default)]
    pub reasoning_sample_count: u32,
}

impl Default for CalibrationData {
    fn default() -> Self {
        Self {
            chars_per_token_prose: 4.0,
            chars_per_token_code: 3.0,
            chars_per_token_mixed: 3.5,
            sample_count: 0,
            reasoning_tokens_ema: None,
            reasoning_sample_count: 0,
        }
    }
}

/// Statistics about tool output (used with spill).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputStats {
    pub total_lines: u64,
    pub total_bytes: u64,
    pub patterns: Vec<(String, u32)>,
}

/// Strategy for truncating large output before spilling to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpillStrategy {
    HeadTail,
    TailOnly,
}

/// Request to the LLM. Borrows system prompt and messages to avoid O(n²) cloning per turn.
#[derive(Debug)]
pub struct CompletionRequest<'a> {
    pub system_blocks: &'a [SystemBlock],
    pub messages: &'a [ChatMessage],
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    /// Hint to providers: tool schemas are stable and may be cached.
    pub cache_tools: bool,
    /// When > 0, tells the provider to enable reasoning/thinking mode.
    pub reserved_reasoning_tokens: u32,
}

/// Response from the LLM (no tools).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub message: ChatMessage,
    pub usage: TokenUsage,
    pub stop_reason: StopReason,
}

/// Response from the LLM with potential tool calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCompletionResponse {
    pub message: ChatMessage,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub stop_reason: StopReason,
}

/// A message in the conversation transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

/// Role of a message sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

/// A content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// Reason the LLM stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    #[serde(untagged)]
    Unknown(String),
}

impl ChatMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl ToolCompletionResponse {
    pub fn text(&self) -> String {
        self.message.text()
    }
}

impl CompletionResponse {
    pub fn text(&self) -> String {
        self.message.text()
    }
}

/// Type of a queue message (used in Type header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageType {
    Task,
    Goal,
    Subtask,
    Continuation,
}

/// Headers for apex queue messages (mapped to rfbmq custom headers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageHeaders {
    pub message_type: MessageType,
    pub correlation_id: String,
    pub depth: u32,
    pub retry_count: u32,
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillManifest>,
}

/// A queue message with headers and markdown body.
#[derive(Debug, Clone)]
pub struct QueueMessage {
    pub headers: MessageHeaders,
    pub body: String,
}

/// A claimed message from the queue (in processing state).
#[derive(Debug, Clone)]
pub struct ClaimedTask {
    pub id: String,
    pub claim_path: String,
    pub headers: MessageHeaders,
    pub body: String,
}

/// Queue depth info.
#[derive(Debug, Clone, Default)]
pub struct QueueDepth {
    pub pending: u32,
    pub processing: u32,
}

/// Reap results.
#[derive(Debug, Clone, Default)]
pub struct ReapResult {
    pub lease_reaped: u32,
}

/// Metadata for a message in a queue directory (for listing/inspection).
#[derive(Debug, Clone)]
pub struct QueueMessageMeta {
    pub id: String,
    pub type_label: String,
    pub correlation_id: String,
    pub depends_on: Vec<String>,
}

// ── Long-Term Memory types ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FactId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: FactId,
    pub content: String,
    pub source_job: String,
    pub confidence: f64,
    pub created_at: String,
    pub last_verified: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillStatus {
    Active,
    Retired,
}

impl std::fmt::Display for SkillStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillStatus::Active => f.write_str("active"),
            SkillStatus::Retired => f.write_str("retired"),
        }
    }
}

/// YAML frontmatter for agentskills.io spec-compliant SKILL.md files.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatibility: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    metadata: BTreeMap<String, String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "allowed-tools"
    )]
    allowed_tools: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    // Spec fields (in frontmatter)
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Option<String>,
    /// Non-apex metadata from other clients, preserved on round-trip.
    #[serde(default)]
    pub extra_metadata: BTreeMap<String, String>,

    // Operational fields (stored in metadata.apex-* keys)
    pub id: SkillId,
    pub task_pattern: String,
    /// Freeform body content (per agentskills.io spec, no format restrictions).
    pub approach: String,
    pub tools_used: Vec<String>,
    pub success_count: u32,
    pub failure_count: u32,
    pub fitness: f64,
    pub min_samples: u32,
    pub last_used: String,
    #[serde(default = "default_skill_status")]
    pub status: SkillStatus,
    /// Semver version string (default "1.0.0").
    #[serde(default = "default_skill_version")]
    pub version: String,
    /// Path to the skill directory (set on load, not serialized).
    #[serde(skip)]
    pub skill_dir: Option<PathBuf>,
}

fn default_skill_version() -> String {
    "1.0.0".to_string()
}

fn default_skill_status() -> SkillStatus {
    SkillStatus::Active
}

fn default_manifest_version() -> String {
    "latest".to_string()
}

/// Lightweight reference handle for a skill — enough for validation and routing,
/// without loading the full approach body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillManifest {
    pub name: String,
    #[serde(default = "default_manifest_version")]
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// A skill that has been fully loaded into memory for use.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub manifest: SkillManifest,
    pub skill: Skill,
}

/// Parse a comma-separated string into a `Vec<String>`, trimming whitespace
/// and filtering empty entries.
pub fn parse_comma_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

impl Skill {
    /// Build a lightweight `SkillManifest` from this skill.
    pub fn to_manifest(&self) -> SkillManifest {
        let allowed_tools = self
            .allowed_tools
            .as_deref()
            .map(parse_comma_list)
            .unwrap_or_default();
        SkillManifest {
            name: self.name.clone(),
            version: self.version.clone(),
            allowed_tools,
            hooks: vec![],
            prompt: None,
            digest: None,
        }
    }
}

/// Convert a string into an agentskills.io spec-compliant name.
///
/// Constraints: lowercase alphanumeric + hyphens only, no consecutive hyphens,
/// no leading/trailing hyphens, max 64 chars (truncated at word boundary).
pub fn slugify(s: &str) -> String {
    let lower = s.to_lowercase();
    let slug: String = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Collapse consecutive dashes and trim leading/trailing
    let mut result = String::new();
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash && !result.is_empty() {
                result.push('-');
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }
    let result = result.trim_end_matches('-').to_string();

    // Enforce max 64 chars, truncating at word (hyphen) boundary
    if result.len() <= 64 {
        return result;
    }
    let truncated = &result[..64];
    match truncated.rfind('-') {
        Some(pos) if pos > 0 => truncated[..pos].to_string(),
        _ => truncated.to_string(),
    }
}

impl Skill {
    /// Serialize this skill to an agentskills.io spec-compliant SKILL.md file.
    ///
    /// Operational fields are stored in the `metadata` map with `apex-` prefix.
    /// Body preserves the structured section format.
    pub fn to_markdown(&self) -> String {
        let name = if self.name.is_empty() {
            slugify(&self.task_pattern)
        } else {
            self.name.clone()
        };
        let description = if self.description.is_empty() {
            self.task_pattern.clone()
        } else {
            self.description.clone()
        };

        // Build metadata map: apex-* operational fields + any extra metadata
        let mut metadata = self.extra_metadata.clone();
        metadata.insert("apex-id".to_string(), self.id.0.clone());
        metadata.insert("apex-task-pattern".to_string(), self.task_pattern.clone());
        if !self.tools_used.is_empty() {
            metadata.insert("apex-tools-used".to_string(), self.tools_used.join(", "));
        }
        metadata.insert(
            "apex-success-count".to_string(),
            self.success_count.to_string(),
        );
        metadata.insert(
            "apex-failure-count".to_string(),
            self.failure_count.to_string(),
        );
        metadata.insert("apex-fitness".to_string(), format!("{:.2}", self.fitness));
        metadata.insert("apex-min-samples".to_string(), self.min_samples.to_string());
        metadata.insert("apex-last-used".to_string(), self.last_used.clone());
        metadata.insert("apex-status".to_string(), self.status.to_string());
        metadata.insert("apex-version".to_string(), self.version.clone());

        let frontmatter = SkillFrontmatter {
            name,
            description,
            license: self.license.clone(),
            compatibility: self.compatibility.clone(),
            metadata,
            allowed_tools: self.allowed_tools.clone(),
        };

        let yaml = yaml_serde::to_string(&frontmatter).unwrap_or_default();
        let mut out = format!("---\n{yaml}---\n\n");

        out.push_str(&self.approach);
        out.push('\n');

        out
    }

    /// Parse a skill from a SKILL.md file with YAML frontmatter.
    ///
    /// Supports both the agentskills.io spec format (metadata map with `apex-*` keys)
    /// and the legacy Apex format (flat frontmatter keys) for backward compatibility.
    pub fn from_markdown(md: &str) -> Result<Self, String> {
        // Split frontmatter from body
        let md = md.trim_start();
        if !md.starts_with("---") {
            return Err("missing frontmatter delimiter".to_string());
        }
        let after_first = &md[3..];
        let end = after_first
            .find("\n---")
            .ok_or("missing closing frontmatter delimiter")?;
        let frontmatter_str = &after_first[..end];
        let body = &after_first[end + 4..]; // skip "\n---"

        // Parse YAML frontmatter via yaml_serde
        let fm = yaml_serde::from_str::<SkillFrontmatter>(frontmatter_str)
            .map_err(|e| format!("invalid YAML frontmatter: {e}"))?;
        let (
            mut name,
            mut description,
            license,
            compatibility,
            allowed_tools,
            id,
            task_pattern,
            tools_used,
            success_count,
            failure_count,
            fitness,
            min_samples,
            last_used,
            status,
            extra_metadata,
            version,
        ) = Self::extract_from_spec_frontmatter(fm);

        // Body is freeform markdown per agentskills.io spec
        let approach = body.trim().to_string();

        // Derive name/description from task_pattern for backward compat
        if name.is_empty() {
            name = slugify(&task_pattern);
        }
        if description.is_empty() {
            description = task_pattern.clone();
        }

        Ok(Skill {
            name,
            description,
            license,
            compatibility,
            allowed_tools,
            extra_metadata,
            id: SkillId(id),
            task_pattern,
            approach,
            tools_used,
            success_count,
            failure_count,
            fitness,
            min_samples,
            last_used,
            status,
            version,
            skill_dir: None,
        })
    }

    /// Extract operational fields from spec-compliant frontmatter with `metadata.apex-*` keys.
    #[allow(clippy::type_complexity)]
    fn extract_from_spec_frontmatter(
        fm: SkillFrontmatter,
    ) -> (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
        Vec<String>,
        u32,
        u32,
        f64,
        u32,
        String,
        SkillStatus,
        BTreeMap<String, String>,
        String,
    ) {
        let id = fm.metadata.get("apex-id").cloned().unwrap_or_default();
        let task_pattern = fm
            .metadata
            .get("apex-task-pattern")
            .cloned()
            .unwrap_or_else(|| fm.description.clone());
        let tools_used = fm
            .metadata
            .get("apex-tools-used")
            .map(|v| parse_comma_list(v))
            .unwrap_or_default();
        let success_count = fm
            .metadata
            .get("apex-success-count")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let failure_count = fm
            .metadata
            .get("apex-failure-count")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let fitness = fm
            .metadata
            .get("apex-fitness")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        let min_samples = fm
            .metadata
            .get("apex-min-samples")
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let last_used = fm
            .metadata
            .get("apex-last-used")
            .cloned()
            .unwrap_or_default();
        let status = match fm.metadata.get("apex-status").map(|s| s.as_str()) {
            Some("retired") => SkillStatus::Retired,
            _ => SkillStatus::Active,
        };
        let version = fm
            .metadata
            .get("apex-version")
            .cloned()
            .unwrap_or_else(default_skill_version);

        // Preserve non-apex metadata keys
        let extra_metadata: BTreeMap<String, String> = fm
            .metadata
            .into_iter()
            .filter(|(k, _)| !k.starts_with("apex-"))
            .collect();

        (
            fm.name,
            fm.description,
            fm.license,
            fm.compatibility,
            fm.allowed_tools,
            id,
            task_pattern,
            tools_used,
            success_count,
            failure_count,
            fitness,
            min_samples,
            last_used,
            status,
            extra_metadata,
            version,
        )
    }
}

/// List resource files in a skill's optional subdirectories (scripts/, references/, assets/).
///
/// Returns a map of directory name to relative file paths within that directory.
pub fn list_skill_resources(skill_dir: &std::path::Path) -> HashMap<String, Vec<String>> {
    let mut resources = HashMap::new();
    for subdir in &["scripts", "references", "assets"] {
        if let Ok(entries) = std::fs::read_dir(skill_dir.join(subdir)) {
            let mut files: Vec<String> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|ft| ft.is_file()))
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .map(|name| format!("{subdir}/{name}"))
                })
                .collect();
            files.sort();
            if !files.is_empty() {
                resources.insert(subdir.to_string(), files);
            }
        }
    }
    resources
}

// ── Working Memory types ──────────────────────────────────────────

/// A single log entry in the scratchpad execution log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub turn: u32,
    pub tool_name: String,
    pub input_summary: String,
    pub output_summary: String,
    pub is_error: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubtaskStatus {
    Done,
    Active,
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtaskEntry {
    pub index: u32,
    pub description: String,
    pub status: SubtaskStatus,
    pub task_id: Option<String>,
    pub depends_on: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scratchpad {
    pub job_id: String,
    pub goal: String,
    pub subtasks: Vec<SubtaskEntry>,
    pub status_summary: String,
    pub notes: Vec<String>,
    #[serde(default)]
    pub log: Vec<LogEntry>,
}

impl Scratchpad {
    pub fn new(job_id: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            job_id: job_id.into(),
            goal: goal.into(),
            subtasks: Vec::new(),
            status_summary: String::new(),
            notes: Vec::new(),
            log: Vec::new(),
        }
    }

    pub fn to_markdown(&self) -> String {
        let mut out = format!(
            "# Working Memory: {}\n\n## Goal\n{}\n\n## Decomposition\n",
            self.job_id, self.goal
        );

        if self.subtasks.is_empty() {
            out.push_str("(none)\n");
        } else {
            for st in &self.subtasks {
                let status = match st.status {
                    SubtaskStatus::Done => "done",
                    SubtaskStatus::Active => "active",
                    SubtaskStatus::Pending => "pending",
                };
                out.push_str(&format!("{}. [{}] {}", st.index, status, st.description));
                if let Some(ref tid) = st.task_id {
                    out.push_str(&format!(" → {tid}"));
                }
                if let Some(ref dep) = st.depends_on {
                    out.push_str(&format!(" (depends on {dep})"));
                }
                out.push('\n');
            }
        }

        out.push_str(&format!("\n## Status\n{}\n", self.status_summary));

        out.push_str("\n## Job-Level Notes\n");
        if self.notes.is_empty() {
            out.push_str("(none)\n");
        } else {
            for note in &self.notes {
                out.push_str(&format!("- {note}\n"));
            }
        }

        out.push_str("\n## Execution Log\n");
        if self.log.is_empty() {
            out.push_str("(none)\n");
        } else {
            for entry in &self.log {
                let status = if entry.is_error { "ERR" } else { "ok" };
                out.push_str(&format!(
                    "- [turn {}] `{}` — {} ({}ms, {})\n",
                    entry.turn, entry.tool_name, entry.input_summary, entry.duration_ms, status,
                ));
            }
        }

        out
    }

    pub fn from_markdown(md: &str) -> Result<Self, String> {
        let mut job_id = String::new();
        let mut goal = String::new();
        let mut subtasks = Vec::new();
        let mut status_summary = String::new();
        let mut notes = Vec::new();
        let mut log = Vec::new();

        #[derive(PartialEq)]
        enum Section {
            None,
            Goal,
            Decomposition,
            Status,
            Notes,
            Log,
        }
        let mut section = Section::None;

        for line in md.lines() {
            if line.starts_with("# Working Memory: ") {
                job_id = line.trim_start_matches("# Working Memory: ").to_string();
                continue;
            }
            if line == "## Goal" {
                section = Section::Goal;
                continue;
            }
            if line == "## Decomposition" {
                section = Section::Decomposition;
                continue;
            }
            if line == "## Status" {
                section = Section::Status;
                continue;
            }
            if line == "## Job-Level Notes" {
                section = Section::Notes;
                continue;
            }
            if line == "## Execution Log" {
                section = Section::Log;
                continue;
            }

            match section {
                Section::Goal => {
                    if !line.is_empty() {
                        if !goal.is_empty() {
                            goal.push('\n');
                        }
                        goal.push_str(line);
                    }
                }
                Section::Decomposition => {
                    if line == "(none)" || line.is_empty() {
                        continue;
                    }
                    // Parse: "1. [done] description → task-id (depends on 003)"
                    if let Some(entry) = parse_subtask_line(line) {
                        subtasks.push(entry);
                    }
                }
                Section::Status => {
                    if !line.is_empty() {
                        if !status_summary.is_empty() {
                            status_summary.push('\n');
                        }
                        status_summary.push_str(line);
                    }
                }
                Section::Notes => {
                    if line == "(none)" || line.is_empty() {
                        continue;
                    }
                    if let Some(note) = line.strip_prefix("- ") {
                        notes.push(note.to_string());
                    }
                }
                Section::Log => {
                    if line == "(none)" || line.is_empty() {
                        continue;
                    }
                    if let Some(entry) = parse_log_line(line) {
                        log.push(entry);
                    }
                }
                Section::None => {}
            }
        }

        if job_id.is_empty() {
            return Err("missing job_id header".to_string());
        }

        Ok(Scratchpad {
            job_id,
            goal,
            subtasks,
            status_summary,
            notes,
            log,
        })
    }
}

fn parse_subtask_line(line: &str) -> Option<SubtaskEntry> {
    // "1. [done] description → task-id (depends on 003)"
    let dot_pos = line.find(". [")?;
    let index: u32 = line[..dot_pos].trim().parse().ok()?;

    let after_dot = &line[dot_pos + 3..]; // after ". ["
    let bracket_end = after_dot.find("] ")?;
    let status_str = &after_dot[..bracket_end];
    let status = match status_str {
        "done" => SubtaskStatus::Done,
        "active" => SubtaskStatus::Active,
        "pending" => SubtaskStatus::Pending,
        _ => return None,
    };

    let rest = &after_dot[bracket_end + 2..]; // after "] "

    // Extract depends_on from end
    let (rest, depends_on) = if let Some(dep_start) = rest.rfind(" (depends on ") {
        let dep_end = rest.len();
        let dep = rest[dep_start + 13..dep_end]
            .trim_end_matches(')')
            .to_string();
        (&rest[..dep_start], Some(dep))
    } else {
        (rest, None)
    };

    // Extract task_id from " → task-id"
    let (description, task_id) = if let Some(arrow_pos) = rest.find(" → ") {
        (
            rest[..arrow_pos].to_string(),
            Some(rest[arrow_pos + 5..].to_string()),
        )
    } else {
        (rest.to_string(), None)
    };

    Some(SubtaskEntry {
        index,
        description,
        status,
        task_id,
        depends_on,
    })
}

/// Parse a log line: "- [turn 1] `shell_exec` — ls -la (250ms, ok)"
fn parse_log_line(line: &str) -> Option<LogEntry> {
    let rest = line.strip_prefix("- [turn ")?;
    let bracket_end = rest.find(']')?;
    let turn: u32 = rest[..bracket_end].parse().ok()?;

    let rest = rest[bracket_end + 2..].strip_prefix('`')?;
    let backtick_end = rest.find('`')?;
    let tool_name = rest[..backtick_end].to_string();

    // After "` — "
    let rest = rest.get(backtick_end + 1..)?.strip_prefix(" — ")?;

    // Find the trailing " (NNNms, ok)" or " (NNNms, ERR)"
    let paren_start = rest.rfind(" (")?;
    let input_summary = rest[..paren_start].to_string();
    let trailer = rest[paren_start + 2..].trim_end_matches(')');

    // "250ms, ok" or "250ms, ERR"
    let comma_pos = trailer.find(", ")?;
    let duration_ms: u64 = trailer[..comma_pos].trim_end_matches("ms").parse().ok()?;
    let is_error = trailer[comma_pos + 2..] == *"ERR";

    Some(LogEntry {
        turn,
        tool_name,
        input_summary,
        output_summary: String::new(), // not stored in markdown format
        is_error,
        duration_ms,
    })
}

// ── Lifecycle Hook types ──────────────────────────────────────────

/// Events at which hooks can fire within the lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    BeforeTurn,
    AfterTurn,
    BeforeToolCall,
    AfterToolResult,
    BeforePush,
    AfterClaim,
    OnSuccess,
    OnFailure,
    OnLog,
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            HookEvent::BeforeTurn => "before_turn",
            HookEvent::AfterTurn => "after_turn",
            HookEvent::BeforeToolCall => "before_tool_call",
            HookEvent::AfterToolResult => "after_tool_result",
            HookEvent::BeforePush => "before_push",
            HookEvent::AfterClaim => "after_claim",
            HookEvent::OnSuccess => "on_success",
            HookEvent::OnFailure => "on_failure",
            HookEvent::OnLog => "on_log",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for HookEvent {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "before_turn" => Ok(HookEvent::BeforeTurn),
            "after_turn" => Ok(HookEvent::AfterTurn),
            "before_tool_call" => Ok(HookEvent::BeforeToolCall),
            "after_tool_result" => Ok(HookEvent::AfterToolResult),
            "before_push" => Ok(HookEvent::BeforePush),
            "after_claim" => Ok(HookEvent::AfterClaim),
            "on_success" => Ok(HookEvent::OnSuccess),
            "on_failure" => Ok(HookEvent::OnFailure),
            "on_log" => Ok(HookEvent::OnLog),
            other => Err(format!("unknown hook event: {other}")),
        }
    }
}

/// What a hook does when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookActionType {
    Script,
    Transform,
    Block,
    Inject,
}

/// What happens when a hook script fails (non-zero exit).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailureBehavior {
    Block,
    #[default]
    Warn,
    Continue,
}

/// Optional filter for when a hook should trigger.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookFilter {
    /// Only fire for this specific tool name (for before_tool_call / after_tool_result).
    #[serde(default)]
    pub tool: Option<String>,
}

/// Action configuration for a hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookAction {
    #[serde(rename = "type")]
    pub action_type: HookActionType,
    /// Command to execute (for script/transform types).
    #[serde(default)]
    pub command: Option<String>,
    /// What to pipe to stdin: "tool_call", "tool_result", "message", "context".
    #[serde(default)]
    pub input: Option<String>,
    /// Timeout in milliseconds for script execution.
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// Content to inject (for inject type).
    #[serde(default)]
    pub content: Option<String>,
    /// What to do when the action fails.
    #[serde(default)]
    pub on_failure: OnFailureBehavior,
    /// Message to use when blocking.
    #[serde(default)]
    pub message: Option<String>,
}

fn default_hook_timeout_ms() -> u64 {
    5000
}

/// A complete hook definition, parsed from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    pub hook: HookMeta,
    pub action: HookAction,
    /// The file path this hook was loaded from (set at load time, not in TOML).
    #[serde(skip)]
    pub source_path: Option<String>,
}

/// Metadata section of a hook definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookMeta {
    pub name: String,
    pub event: HookEvent,
    #[serde(default)]
    pub filter: HookFilter,
    #[serde(default = "default_hook_priority")]
    pub priority: i32,
    /// If true, the agent cannot modify or delete this hook.
    #[serde(default)]
    pub invariant: bool,
    /// If true, this hook propagates to sub-agents spawned via delegate.
    #[serde(default)]
    pub propagate: bool,
}

fn default_hook_priority() -> i32 {
    50
}

/// The result of running a hook action.
#[derive(Debug, Clone)]
pub enum HookOutcome {
    /// Hook completed successfully, optionally with transformed data.
    Continue(Option<String>),
    /// Hook says to block the event from proceeding.
    Block(String),
    /// Hook wants to inject content into context.
    Inject(String),
}

/// An attempt record capturing a full execution attempt for a task.
#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub attempt_number: u32,
    pub started_at: String,
    pub finished_at: String,
    pub turns: Vec<TurnRecord>,
    pub final_text: Option<String>,
    pub outcome: AttemptOutcome,
    pub failure_reason: Option<String>,
}

/// A single LLM turn within an attempt.
#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub tool_calls: Vec<ToolCallRecord>,
    pub usage: TokenUsage,
}

/// A tool call record with timing.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub input_summary: String,
    pub output_summary: String,
    pub is_error: bool,
    pub duration_ms: u64,
}

/// Outcome of an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    Success,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json, to_value};

    // ── ChatMessage::user_text ──────────────────────────────────────

    #[test]
    fn user_text_creates_user_message_with_single_text_block() {
        let msg = ChatMessage::user_text("hello");
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content.len(), 1);
        match &msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello"),
            other => panic!("expected Text block, got {:?}", other),
        }
    }

    #[test]
    fn user_text_accepts_string() {
        let msg = ChatMessage::user_text(String::from("owned"));
        assert_eq!(msg.text(), "owned");
    }

    // ── ChatMessage::text ───────────────────────────────────────────

    #[test]
    fn text_extracts_single_text_block() {
        let msg = ChatMessage::user_text("only");
        assert_eq!(msg.text(), "only");
    }

    #[test]
    fn text_joins_multiple_text_blocks_with_newlines() {
        let msg = ChatMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "line1".into(),
                },
                ContentBlock::Text {
                    text: "line2".into(),
                },
            ],
        };
        assert_eq!(msg.text(), "line1\nline2");
    }

    #[test]
    fn text_ignores_non_text_blocks() {
        let msg = ChatMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "before".into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "tool".into(),
                    input: json!({}),
                },
                ContentBlock::Text {
                    text: "after".into(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "result".into(),
                    is_error: false,
                },
            ],
        };
        assert_eq!(msg.text(), "before\nafter");
    }

    #[test]
    fn text_returns_empty_when_no_text_blocks() {
        let msg = ChatMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "res".into(),
                is_error: false,
            }],
        };
        assert_eq!(msg.text(), "");
    }

    #[test]
    fn text_returns_empty_for_empty_content() {
        let msg = ChatMessage {
            role: MessageRole::User,
            content: vec![],
        };
        assert_eq!(msg.text(), "");
    }

    // ── MessageRole serde ───────────────────────────────────────────

    #[test]
    fn message_role_serializes_lowercase() {
        assert_eq!(to_value(MessageRole::User).unwrap(), json!("user"));
        assert_eq!(
            to_value(MessageRole::Assistant).unwrap(),
            json!("assistant")
        );
    }

    #[test]
    fn message_role_roundtrips() {
        let user: MessageRole = from_value(json!("user")).unwrap();
        let assistant: MessageRole = from_value(json!("assistant")).unwrap();
        assert_eq!(user, MessageRole::User);
        assert_eq!(assistant, MessageRole::Assistant);
    }

    // ── ContentBlock serde ──────────────────────────────────────────

    #[test]
    fn content_block_text_serializes() {
        let block = ContentBlock::Text { text: "hi".into() };
        let v = to_value(&block).unwrap();
        assert_eq!(v, json!({"type": "text", "text": "hi"}));
    }

    #[test]
    fn content_block_text_roundtrips() {
        let v = json!({"type": "text", "text": "hi"});
        let block: ContentBlock = from_value(v.clone()).unwrap();
        assert_eq!(to_value(&block).unwrap(), v);
    }

    #[test]
    fn content_block_tool_use_serializes() {
        let block = ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read_file".into(),
            input: json!({"path": "/tmp"}),
        };
        let v = to_value(&block).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "tool_use",
                "id": "call_1",
                "name": "read_file",
                "input": {"path": "/tmp"}
            })
        );
    }

    #[test]
    fn content_block_tool_use_roundtrips() {
        let v = json!({
            "type": "tool_use",
            "id": "call_1",
            "name": "read_file",
            "input": {"path": "/tmp"}
        });
        let block: ContentBlock = from_value(v.clone()).unwrap();
        assert_eq!(to_value(&block).unwrap(), v);
    }

    #[test]
    fn content_block_tool_result_serializes() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "done".into(),
            is_error: false,
        };
        let v = to_value(&block).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": "done",
                "is_error": false
            })
        );
    }

    #[test]
    fn content_block_tool_result_roundtrips() {
        let v = json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "done",
            "is_error": true
        });
        let block: ContentBlock = from_value(v.clone()).unwrap();
        assert_eq!(to_value(&block).unwrap(), v);
    }

    #[test]
    fn content_block_tool_result_is_error_defaults_to_false() {
        let v = json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "ok"
        });
        let block: ContentBlock = from_value(v).unwrap();
        match block {
            ContentBlock::ToolResult { is_error, .. } => assert!(!is_error),
            other => panic!("expected ToolResult, got {:?}", other),
        }
    }

    // ── StopReason serde ────────────────────────────────────────────

    #[test]
    fn stop_reason_serializes() {
        assert_eq!(to_value(&StopReason::EndTurn).unwrap(), json!("end_turn"));
        assert_eq!(to_value(&StopReason::ToolUse).unwrap(), json!("tool_use"));
        assert_eq!(
            to_value(&StopReason::MaxTokens).unwrap(),
            json!("max_tokens")
        );
        assert_eq!(
            to_value(&StopReason::StopSequence).unwrap(),
            json!("stop_sequence")
        );
    }

    #[test]
    fn stop_reason_roundtrips_known_variants() {
        for (s, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
        ] {
            let got: StopReason = from_value(json!(s)).unwrap();
            assert_eq!(got, expected);
        }
    }

    #[test]
    fn stop_reason_unknown_roundtrips() {
        let unknown: StopReason = from_value(json!("content_filter")).unwrap();
        assert_eq!(unknown, StopReason::Unknown("content_filter".into()));
        assert_eq!(to_value(&unknown).unwrap(), json!("content_filter"));
    }

    // ── TokenUsage default ──────────────────────────────────────────

    #[test]
    fn token_usage_defaults_to_zero() {
        let usage = TokenUsage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    // ── CompletionResponse::text / ToolCompletionResponse::text ─────

    #[test]
    fn completion_response_text_delegates() {
        let resp = CompletionResponse {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "hello".into(),
                    },
                    ContentBlock::Text {
                        text: "world".into(),
                    },
                ],
            },
            usage: TokenUsage::default(),
            stop_reason: StopReason::EndTurn,
        };
        assert_eq!(resp.text(), "hello\nworld");
    }

    #[test]
    fn tool_completion_response_text_delegates() {
        let resp = ToolCompletionResponse {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "result".into(),
                }],
            },
            tool_calls: vec![],
            usage: TokenUsage::default(),
            stop_reason: StopReason::EndTurn,
        };
        assert_eq!(resp.text(), "result");
    }

    #[test]
    fn tool_completion_response_text_ignores_tool_blocks() {
        let resp = ToolCompletionResponse {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "thinking".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "bash".into(),
                        input: json!({"cmd": "ls"}),
                    },
                ],
            },
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: json!({"cmd": "ls"}),
            }],
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            stop_reason: StopReason::ToolUse,
        };
        assert_eq!(resp.text(), "thinking");
    }

    // ── SubtaskStatus serde ─────────────────────────────────────────

    #[test]
    fn subtask_status_serializes_lowercase() {
        assert_eq!(to_value(SubtaskStatus::Done).unwrap(), json!("done"));
        assert_eq!(to_value(SubtaskStatus::Active).unwrap(), json!("active"));
        assert_eq!(to_value(SubtaskStatus::Pending).unwrap(), json!("pending"));
    }

    #[test]
    fn subtask_status_roundtrips() {
        for (s, expected) in [
            ("done", SubtaskStatus::Done),
            ("active", SubtaskStatus::Active),
            ("pending", SubtaskStatus::Pending),
        ] {
            let got: SubtaskStatus = from_value(json!(s)).unwrap();
            assert_eq!(got, expected);
        }
    }

    // ── Scratchpad to_markdown / from_markdown roundtrip ────────────

    #[test]
    fn scratchpad_roundtrip_full() {
        let pad = Scratchpad {
            job_id: "job-42".into(),
            goal: "Deploy the widget".into(),
            subtasks: vec![
                SubtaskEntry {
                    index: 1,
                    description: "Build artifact".into(),
                    status: SubtaskStatus::Done,
                    task_id: Some("task-001".into()),
                    depends_on: None,
                },
                SubtaskEntry {
                    index: 2,
                    description: "Run tests".into(),
                    status: SubtaskStatus::Active,
                    task_id: Some("task-002".into()),
                    depends_on: Some("001".into()),
                },
                SubtaskEntry {
                    index: 3,
                    description: "Deploy".into(),
                    status: SubtaskStatus::Pending,
                    task_id: None,
                    depends_on: Some("002".into()),
                },
            ],
            status_summary: "Step 1 done, step 2 running".into(),
            notes: vec!["Found a config issue".into(), "Retrying with fix".into()],
            log: vec![
                LogEntry {
                    turn: 1,
                    tool_name: "shell_exec".into(),
                    input_summary: "ls -la".into(),
                    output_summary: "file1 file2".into(),
                    is_error: false,
                    duration_ms: 250,
                },
                LogEntry {
                    turn: 2,
                    tool_name: "file_write".into(),
                    input_summary: "/tmp/out.txt".into(),
                    output_summary: "ok".into(),
                    is_error: true,
                    duration_ms: 15,
                },
            ],
        };

        let md = pad.to_markdown();
        let parsed = Scratchpad::from_markdown(&md).unwrap();

        assert_eq!(parsed.job_id, "job-42");
        assert_eq!(parsed.goal, "Deploy the widget");
        assert_eq!(parsed.subtasks.len(), 3);
        assert_eq!(parsed.subtasks[0].status, SubtaskStatus::Done);
        assert_eq!(parsed.subtasks[0].task_id.as_deref(), Some("task-001"));
        assert_eq!(parsed.subtasks[1].depends_on.as_deref(), Some("001"));
        assert_eq!(parsed.subtasks[2].task_id, None);
        assert_eq!(parsed.status_summary, "Step 1 done, step 2 running");
        assert_eq!(
            parsed.notes,
            vec!["Found a config issue", "Retrying with fix"]
        );

        // Verify log roundtrip
        assert_eq!(parsed.log.len(), 2);
        assert_eq!(parsed.log[0].turn, 1);
        assert_eq!(parsed.log[0].tool_name, "shell_exec");
        assert_eq!(parsed.log[0].input_summary, "ls -la");
        assert!(!parsed.log[0].is_error);
        assert_eq!(parsed.log[0].duration_ms, 250);
        assert_eq!(parsed.log[1].turn, 2);
        assert_eq!(parsed.log[1].tool_name, "file_write");
        assert!(parsed.log[1].is_error);
        assert_eq!(parsed.log[1].duration_ms, 15);
    }

    #[test]
    fn scratchpad_roundtrip_empty() {
        let pad = Scratchpad::new("job-00", "");
        let md = pad.to_markdown();
        let parsed = Scratchpad::from_markdown(&md).unwrap();

        assert_eq!(parsed.job_id, "job-00");
        assert!(parsed.goal.is_empty());
        assert!(parsed.subtasks.is_empty());
        assert!(parsed.notes.is_empty());
        assert!(parsed.log.is_empty());
    }

    #[test]
    fn scratchpad_from_markdown_missing_header() {
        let result = Scratchpad::from_markdown("## Goal\nSomething\n");
        assert!(result.is_err());
    }

    #[test]
    fn scratchpad_new_creates_empty() {
        let pad = Scratchpad::new("job-99", "Fix the bug");
        assert_eq!(pad.job_id, "job-99");
        assert_eq!(pad.goal, "Fix the bug");
        assert!(pad.subtasks.is_empty());
        assert!(pad.status_summary.is_empty());
        assert!(pad.notes.is_empty());
        assert!(pad.log.is_empty());
    }

    // ── Skill spec-compliant format ─────────────────────────────────

    #[test]
    fn skill_round_trip_spec_format() {
        let skill = Skill {
            name: "deploy-app".to_string(),
            description: "Deploy app to production".to_string(),
            license: Some("MIT".to_string()),
            compatibility: None,
            allowed_tools: Some("shell_exec, file_read".to_string()),
            extra_metadata: BTreeMap::from([("custom-key".to_string(), "custom-val".to_string())]),
            id: SkillId("skill-abc123".to_string()),
            task_pattern: "deploy app to production".to_string(),
            approach: "1. Run tests\n2. Build release".to_string(),
            tools_used: vec!["shell_exec".to_string(), "file_read".to_string()],
            success_count: 5,
            failure_count: 1,
            fitness: 0.83,
            min_samples: 3,
            last_used: "1707123456".to_string(),
            status: SkillStatus::Active,
            version: "1.0.0".to_string(),
            skill_dir: None,
        };

        let md = skill.to_markdown();
        assert!(md.starts_with("---\n"));
        assert!(md.contains("name: deploy-app"));
        assert!(md.contains("apex-id: skill-abc123"));
        assert!(md.contains("apex-task-pattern: deploy app to production"));
        assert!(md.contains("custom-key: custom-val"));
        assert!(md.contains("license: MIT"));

        let parsed = Skill::from_markdown(&md).unwrap();
        assert_eq!(parsed.name, "deploy-app");
        assert_eq!(parsed.description, "Deploy app to production");
        assert_eq!(parsed.license, Some("MIT".to_string()));
        assert_eq!(parsed.id.0, "skill-abc123");
        assert_eq!(parsed.task_pattern, "deploy app to production");
        assert_eq!(parsed.tools_used, vec!["shell_exec", "file_read"]);
        assert_eq!(parsed.success_count, 5);
        assert_eq!(parsed.failure_count, 1);
        assert!((parsed.fitness - 0.83).abs() < 0.01);
        assert_eq!(parsed.approach, "1. Run tests\n2. Build release");
        assert_eq!(parsed.status, SkillStatus::Active);
        assert_eq!(
            parsed.extra_metadata.get("custom-key").unwrap(),
            "custom-val"
        );
    }

    #[test]
    fn skill_freeform_body_round_trip() {
        let freeform_body = "# Getting Started\n\nRun `cargo build` first.\n\n## Tips\n\n- Use `--release` for production\n- Check logs in `/var/log`";
        let skill = Skill {
            name: "freeform-skill".to_string(),
            description: "A skill with freeform body".to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            extra_metadata: Default::default(),
            id: SkillId("skill-free".to_string()),
            task_pattern: "freeform task".to_string(),
            approach: freeform_body.to_string(),
            tools_used: vec![],
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            min_samples: 3,
            last_used: String::new(),
            status: SkillStatus::Active,
            version: "1.0.0".to_string(),
            skill_dir: None,
        };

        let md = skill.to_markdown();
        let parsed = Skill::from_markdown(&md).unwrap();
        assert_eq!(parsed.approach, freeform_body);
    }

    #[test]
    fn skill_authored_no_apex_id() {
        let authored = "\
---
name: my-authored-skill
description: An authored skill for testing
---

Just do the thing.
";
        let skill = Skill::from_markdown(authored).unwrap();
        assert_eq!(skill.name, "my-authored-skill");
        assert_eq!(skill.description, "An authored skill for testing");
        assert!(skill.id.0.is_empty()); // no apex-id
        assert_eq!(skill.task_pattern, "An authored skill for testing"); // derived from description
        assert_eq!(skill.approach, "Just do the thing.");
    }

    // ── slugify spec compliance ─────────────────────────────────────

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Deploy App"), "deploy-app");
        assert_eq!(slugify("hello   world"), "hello-world");
        assert_eq!(slugify("--leading--trailing--"), "leading-trailing");
    }

    #[test]
    fn slugify_max_64_chars() {
        let long = "a".repeat(100);
        let result = slugify(&long);
        assert!(result.len() <= 64);
    }

    #[test]
    fn slugify_truncates_at_word_boundary() {
        // 65 chars with a hyphen near the boundary
        let input = format!("{}-{}", "a".repeat(50), "b".repeat(14));
        let result = slugify(&input);
        assert!(result.len() <= 64);
        assert!(!result.ends_with('-'));
    }

    #[test]
    fn slugify_no_consecutive_hyphens() {
        let result = slugify("hello---world");
        assert!(!result.contains("--"));
    }

    #[test]
    fn skill_empty_body() {
        let md = "---\nname: empty\ndescription: No body\n---\n";
        let skill = Skill::from_markdown(md).unwrap();
        assert_eq!(skill.approach, "");
    }

    #[test]
    fn skill_body_with_horizontal_rule() {
        let md = "---\nname: hr-test\ndescription: Body has hr\n---\n\nBefore rule\n\n---\n\nAfter rule\n";
        let skill = Skill::from_markdown(md).unwrap();
        assert!(skill.approach.contains("Before rule"));
        assert!(skill.approach.contains("---"));
        assert!(skill.approach.contains("After rule"));
    }
}
