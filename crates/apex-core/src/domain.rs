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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub name: String,
    pub output: Value,
    pub is_error: bool,
}

/// Token usage from an LLM response.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
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
}
