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

/// Request to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub system_prompt: String,
    pub messages: Vec<ChatMessage>,
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

// ── Long-Term Memory types ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FactId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StrategyId(pub String);

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strategy {
    pub id: StrategyId,
    pub goal_pattern: String,
    pub decomposition: String,
    pub avg_subtasks: f64,
    pub avg_duration_secs: f64,
    pub success_count: u32,
    pub failure_count: u32,
    pub fitness: f64,
    pub notes: String,
}

// ── Working Memory types ──────────────────────────────────────────

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
}

impl Scratchpad {
    pub fn new(job_id: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            job_id: job_id.into(),
            goal: goal.into(),
            subtasks: Vec::new(),
            status_summary: String::new(),
            notes: Vec::new(),
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

        out
    }

    pub fn from_markdown(md: &str) -> Result<Self, String> {
        let mut job_id = String::new();
        let mut goal = String::new();
        let mut subtasks = Vec::new();
        let mut status_summary = String::new();
        let mut notes = Vec::new();

        #[derive(PartialEq)]
        enum Section { None, Goal, Decomposition, Status, Notes }
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
                Section::None => {}
            }
        }

        if job_id.is_empty() {
            return Err("missing job_id header".to_string());
        }

        Ok(Scratchpad { job_id, goal, subtasks, status_summary, notes })
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
    pub eval_summary: Option<String>,
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
    }
}
