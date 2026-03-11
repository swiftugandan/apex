use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON schema for a tool, sent to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A tool definition (schema + metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub schema: ToolSchema,
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

/// Token usage from an LLM response.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
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
}

impl Default for CalibrationData {
    fn default() -> Self {
        Self {
            chars_per_token_prose: 4.0,
            chars_per_token_code: 3.0,
            chars_per_token_mixed: 3.5,
            sample_count: 0,
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
    pub system_prompt: &'a str,
    pub messages: &'a [ChatMessage],
    pub max_tokens: u32,
    pub temperature: Option<f32>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub description: String,
    pub task_pattern: String,
    pub approach: String,
    pub tools_used: Vec<String>,
    pub criteria_template: Option<String>,
    pub success_count: u32,
    pub failure_count: u32,
    pub fitness: f64,
    pub min_samples: u32,
    pub last_used: String,
    pub notes: String,
    #[serde(default = "default_skill_status")]
    pub status: SkillStatus,
}

fn default_skill_status() -> SkillStatus {
    SkillStatus::Active
}

/// Convert a task_pattern into a filename-safe slug.
pub fn slugify(s: &str) -> String {
    let lower = s.to_lowercase();
    let slug: String = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Collapse consecutive dashes and trim
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
    result.trim_end_matches('-').to_string()
}

impl Skill {
    /// Serialize this skill to a markdown file with YAML frontmatter.
    pub fn to_markdown(&self) -> String {
        let tools_str = format!("[{}]", self.tools_used.join(", "));
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
        let mut out = format!(
            "---\n\
             name: {}\n\
             description: \"{}\"\n\
             id: {}\n\
             task_pattern: \"{}\"\n\
             tools_used: {}\n\
             success_count: {}\n\
             failure_count: {}\n\
             fitness: {:.2}\n\
             min_samples: {}\n\
             last_used: \"{}\"\n\
             status: {}\n\
             ---\n\n",
            name,
            description.replace('"', "\\\""),
            self.id.0,
            self.task_pattern.replace('"', "\\\""),
            tools_str,
            self.success_count,
            self.failure_count,
            self.fitness,
            self.min_samples,
            self.last_used,
            self.status,
        );

        out.push_str("## Approach\n\n");
        out.push_str(&self.approach);
        out.push_str("\n\n");

        if let Some(ref criteria) = self.criteria_template {
            out.push_str("## Acceptance Criteria\n\n");
            out.push_str(criteria);
            out.push_str("\n\n");
        }

        if !self.notes.is_empty() {
            out.push_str("## Notes\n\n");
            out.push_str(&self.notes);
            out.push('\n');
        }

        out
    }

    /// Parse a skill from a markdown file with YAML frontmatter.
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
        let frontmatter = &after_first[..end];
        let body = &after_first[end + 4..]; // skip "\n---"

        // Parse frontmatter key-value pairs
        let mut id = String::new();
        let mut name = String::new();
        let mut description = String::new();
        let mut task_pattern = String::new();
        let mut tools_used: Vec<String> = Vec::new();
        let mut success_count: u32 = 0;
        let mut failure_count: u32 = 0;
        let mut fitness: f64 = 0.0;
        let mut min_samples: u32 = 3;
        let mut last_used = String::new();
        let mut status = SkillStatus::Active;

        for line in frontmatter.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim().trim_matches('"');
                match key {
                    "id" => id = value.to_string(),
                    "name" => name = value.to_string(),
                    "description" => description = value.replace("\\\"", "\""),
                    "task_pattern" => task_pattern = value.replace("\\\"", "\""),
                    "tools_used" => {
                        // Parse "[bash, shell_exec]"
                        let inner = value.trim_start_matches('[').trim_end_matches(']');
                        if !inner.is_empty() {
                            tools_used = inner
                                .split(',')
                                .map(|s| s.trim().trim_matches('"').to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                        }
                    }
                    "success_count" => success_count = value.parse().unwrap_or(0),
                    "failure_count" => failure_count = value.parse().unwrap_or(0),
                    "fitness" => fitness = value.parse().unwrap_or(0.0),
                    "min_samples" => min_samples = value.parse().unwrap_or(3),
                    "last_used" => last_used = value.to_string(),
                    "status" => {
                        status = match value {
                            "retired" => SkillStatus::Retired,
                            _ => SkillStatus::Active,
                        }
                    }
                    _ => {}
                }
            }
        }

        if id.is_empty() {
            return Err("missing id in frontmatter".to_string());
        }

        // Parse body sections
        let mut approach = String::new();
        let mut criteria_template: Option<String> = None;
        let mut notes = String::new();

        #[derive(PartialEq)]
        enum Section {
            None,
            Approach,
            Criteria,
            Notes,
        }
        let mut section = Section::None;

        for line in body.lines() {
            if line.starts_with("## Approach") {
                section = Section::Approach;
                continue;
            }
            if line.starts_with("## Acceptance Criteria") {
                section = Section::Criteria;
                criteria_template = Some(String::new());
                continue;
            }
            if line.starts_with("## Notes") {
                section = Section::Notes;
                continue;
            }
            fn append_line(buf: &mut String, line: &str) {
                if !buf.is_empty() || !line.is_empty() {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(line);
                }
            }
            match section {
                Section::Approach => append_line(&mut approach, line),
                Section::Criteria => {
                    if let Some(ref mut c) = criteria_template {
                        append_line(c, line);
                    }
                }
                Section::Notes => append_line(&mut notes, line),
                Section::None => {}
            }
        }

        // Trim trailing whitespace from parsed sections
        let approach = approach.trim_end().to_string();
        let criteria_template = criteria_template.map(|c| c.trim_end().to_string()).filter(|c| !c.is_empty());
        let notes = notes.trim_end().to_string();

        // Derive name/description from task_pattern for backward compat
        if name.is_empty() {
            name = slugify(&task_pattern);
        }
        if description.is_empty() {
            description = task_pattern.clone();
        }

        Ok(Skill {
            id: SkillId(id),
            name,
            description,
            task_pattern,
            approach,
            tools_used,
            criteria_template,
            success_count,
            failure_count,
            fitness,
            min_samples,
            last_used,
            notes,
            status,
        })
    }
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
        let mut out = format!("# Working Memory: {}\n\n## Goal\n{}\n\n## Decomposition\n", self.job_id, self.goal);

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
                    entry.turn, entry.tool_name, entry.input_summary,
                    entry.duration_ms, status,
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
        enum Section { None, Goal, Decomposition, Status, Notes, Log }
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

        Ok(Scratchpad { job_id, goal, subtasks, status_summary, notes, log })
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
        let dep = rest[dep_start + 13..dep_end].trim_end_matches(')').to_string();
        (&rest[..dep_start], Some(dep))
    } else {
        (rest, None)
    };

    // Extract task_id from " → task-id"
    let (description, task_id) = if let Some(arrow_pos) = rest.find(" → ") {
        (rest[..arrow_pos].to_string(), Some(rest[arrow_pos + 5..].to_string()))
    } else {
        (rest.to_string(), None)
    };

    Some(SubtaskEntry { index, description, status, task_id, depends_on })
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailureBehavior {
    Block,
    Warn,
    Continue,
}

impl Default for OnFailureBehavior {
    fn default() -> Self {
        OnFailureBehavior::Warn
    }
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
        assert_eq!(parsed.notes, vec!["Found a config issue", "Retrying with fix"]);

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
}
