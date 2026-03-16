use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use apex_core::domain::{
    CacheHint, ChatMessage, CompletionRequest, CompletionResponse, ContentBlock, MessageRole,
    StopReason, SystemBlock, TokenUsage, ToolCall, ToolCompletionResponse, ToolSchema,
};
use apex_core::error::LlmError;
use apex_core::ports::LlmProvider;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_CONTEXT_WINDOW: usize = 200_000;

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
    context_window: usize,
}

impl AnthropicProvider {
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        context_window: usize,
    ) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            context_window,
        }
    }

    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            LlmError::Configuration("ANTHROPIC_API_KEY environment variable must be set".into())
        })?;
        let model = std::env::var("APEX_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let context_window = std::env::var("APEX_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        Ok(Self::new(api_key, model, context_window))
    }

    /// Create from an explicit model name, reading only the API key from the environment.
    ///
    /// `config_context_window`: optional override from agent.toml `context_window` field.
    /// Priority: config value > APEX_CONTEXT_WINDOW env var > provider default.
    pub fn from_env_with_model(
        model: impl Into<String>,
        config_context_window: Option<usize>,
    ) -> Result<Self, LlmError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
            LlmError::Configuration("ANTHROPIC_API_KEY environment variable must be set".into())
        })?;
        let context_window = config_context_window
            .or_else(|| {
                std::env::var("APEX_CONTEXT_WINDOW")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        Ok(Self::new(api_key, model, context_window))
    }

    async fn send_request(&self, body: Value, use_cache: bool) -> Result<ApiResponse, LlmError> {
        let mut request = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json");

        if use_cache {
            request = request.header("anthropic-beta", "prompt-caching-2024-07-31");
        }

        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "failed to read body".to_string());
            return Err(LlmError::Api(format!("{status}: {body}")));
        }

        response
            .json::<ApiResponse>()
            .await
            .map_err(|e| LlmError::Serialization(e.to_string()))
    }

    fn build_messages(messages: &[ChatMessage]) -> Vec<ApiMessage> {
        messages.iter().map(ApiMessage::from_domain).collect()
    }

    fn build_system(blocks: &[SystemBlock]) -> Value {
        if blocks.len() == 1 && blocks[0].cache_hint == CacheHint::Dynamic {
            serde_json::json!(blocks[0].text)
        } else {
            serde_json::json!(blocks
                .iter()
                .map(|b| {
                    let mut obj = serde_json::json!({ "type": "text", "text": b.text });
                    if b.cache_hint == CacheHint::Static {
                        obj["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                    }
                    obj
                })
                .collect::<Vec<_>>())
        }
    }

    fn build_tools(tools: &[ToolSchema], cache: bool) -> Vec<ApiTool> {
        let len = tools.len();
        tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let cache_control = if cache && i == len - 1 {
                    Some(serde_json::json!({ "type": "ephemeral" }))
                } else {
                    None
                };
                ApiTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.input_schema.clone(),
                    cache_control,
                }
            })
            .collect()
    }

    fn parse_response_message(response: &ApiResponse) -> ChatMessage {
        let content = response
            .content
            .iter()
            .map(|block| match block {
                ApiContentBlock::Text { text } => ContentBlock::Text { text: text.clone() },
                ApiContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
            })
            .collect();

        ChatMessage {
            role: MessageRole::Assistant,
            content,
        }
    }

    fn parse_tool_calls(response: &ApiResponse) -> Vec<ToolCall> {
        response
            .content
            .iter()
            .filter_map(|block| match block {
                ApiContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    fn parse_stop_reason(reason: &str) -> StopReason {
        match reason {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            other => StopReason::Unknown(other.to_string()),
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn complete(&self, req: CompletionRequest<'_>) -> Result<CompletionResponse, LlmError> {
        let use_cache = req
            .system_blocks
            .iter()
            .any(|b| b.cache_hint == CacheHint::Static);
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "system": Self::build_system(req.system_blocks),
            "messages": Self::build_messages(req.messages),
        });

        let response = self.send_request(body, use_cache).await?;
        let message = Self::parse_response_message(&response);
        let stop_reason = Self::parse_stop_reason(&response.stop_reason);

        Ok(CompletionResponse {
            message,
            usage: TokenUsage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
                cache_read_input_tokens: response.usage.cache_read_input_tokens,
                output_tokens_details: None,
            },
            stop_reason,
        })
    }

    async fn complete_with_tools(
        &self,
        req: CompletionRequest<'_>,
        tools: &[ToolSchema],
    ) -> Result<ToolCompletionResponse, LlmError> {
        let use_cache = req
            .system_blocks
            .iter()
            .any(|b| b.cache_hint == CacheHint::Static)
            || req.cache_tools;
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "system": Self::build_system(req.system_blocks),
            "messages": Self::build_messages(req.messages),
            "tools": Self::build_tools(tools, req.cache_tools),
            "tool_choice": { "type": "auto" },
        });

        let response = self.send_request(body, use_cache).await?;
        let message = Self::parse_response_message(&response);
        let tool_calls = Self::parse_tool_calls(&response);
        let stop_reason = Self::parse_stop_reason(&response.stop_reason);

        Ok(ToolCompletionResponse {
            message,
            tool_calls,
            usage: TokenUsage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
                cache_read_input_tokens: response.usage.cache_read_input_tokens,
                output_tokens_details: None,
            },
            stop_reason,
        })
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn context_window(&self) -> usize {
        self.context_window
    }
}

// ── Anthropic-specific serde types (internal only) ──

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: String,
    content: Vec<ApiContentBlockOut>,
}

impl ApiMessage {
    fn from_domain(msg: &ChatMessage) -> Self {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };
        let content = msg
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => ApiContentBlockOut::Text {
                    r#type: "text".to_string(),
                    text: text.clone(),
                },
                ContentBlock::ToolUse { id, name, input } => ApiContentBlockOut::ToolUse {
                    r#type: "tool_use".to_string(),
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => ApiContentBlockOut::ToolResult {
                    r#type: "tool_result".to_string(),
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                },
            })
            .collect();
        Self {
            role: role.to_string(),
            content,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ApiContentBlockOut {
    Text {
        r#type: String,
        text: String,
    },
    ToolUse {
        r#type: String,
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        r#type: String,
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Serialize)]
struct ApiTool {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    content: Vec<ApiContentBlock>,
    stop_reason: String,
    usage: ApiUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ApiContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialize_api_response_text_only() {
        let raw = json!({
            "content": [{"type": "text", "text": "Hello world"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let resp: ApiResponse = serde_json::from_value(raw).unwrap();

        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ApiContentBlock::Text { text } => assert_eq!(text, "Hello world"),
            other => panic!("expected Text block, got {other:?}"),
        }
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn deserialize_api_response_tool_use() {
        let raw = json!({
            "content": [
                {"type": "text", "text": "I'll run that command."},
                {"type": "tool_use", "id": "toolu_123", "name": "shell_exec", "input": {"command": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "output_tokens": 15}
        });

        let resp: ApiResponse = serde_json::from_value(raw).unwrap();

        assert_eq!(resp.content.len(), 2);
        match &resp.content[0] {
            ApiContentBlock::Text { text } => assert_eq!(text, "I'll run that command."),
            other => panic!("expected Text block, got {other:?}"),
        }
        match &resp.content[1] {
            ApiContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_123");
                assert_eq!(name, "shell_exec");
                assert_eq!(input, &json!({"command": "ls"}));
            }
            other => panic!("expected ToolUse block, got {other:?}"),
        }
        assert_eq!(resp.stop_reason, "tool_use");
        assert_eq!(resp.usage.input_tokens, 20);
        assert_eq!(resp.usage.output_tokens, 15);
    }

    #[test]
    fn parse_response_message_produces_assistant_with_correct_blocks() {
        let resp = ApiResponse {
            content: vec![
                ApiContentBlock::Text {
                    text: "thinking...".into(),
                },
                ApiContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "/tmp/f"}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: ApiUsage {
                input_tokens: 1,
                output_tokens: 2,
                ..Default::default()
            },
        };

        let msg = AnthropicProvider::parse_response_message(&resp);

        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.content.len(), 2);
        match &msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "thinking..."),
            other => panic!("expected Text, got {other:?}"),
        }
        match &msg.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read_file");
                assert_eq!(input, &json!({"path": "/tmp/f"}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_calls_extracts_tool_use_blocks() {
        let resp = ApiResponse {
            content: vec![
                ApiContentBlock::Text {
                    text: "here we go".into(),
                },
                ApiContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "shell_exec".into(),
                    input: json!({"command": "pwd"}),
                },
                ApiContentBlock::ToolUse {
                    id: "call_2".into(),
                    name: "read_file".into(),
                    input: json!({"path": "a.txt"}),
                },
            ],
            stop_reason: "tool_use".into(),
            usage: ApiUsage {
                input_tokens: 5,
                output_tokens: 10,
                ..Default::default()
            },
        };

        let calls = AnthropicProvider::parse_tool_calls(&resp);

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input, json!({"command": "pwd"}));
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(calls[1].name, "read_file");
        assert_eq!(calls[1].input, json!({"path": "a.txt"}));
    }

    #[test]
    fn parse_stop_reason_all_variants() {
        assert_eq!(
            AnthropicProvider::parse_stop_reason("end_turn"),
            StopReason::EndTurn
        );
        assert_eq!(
            AnthropicProvider::parse_stop_reason("tool_use"),
            StopReason::ToolUse
        );
        assert_eq!(
            AnthropicProvider::parse_stop_reason("max_tokens"),
            StopReason::MaxTokens
        );
        assert_eq!(
            AnthropicProvider::parse_stop_reason("stop_sequence"),
            StopReason::StopSequence
        );
        assert_eq!(
            AnthropicProvider::parse_stop_reason("unknown_value"),
            StopReason::Unknown("unknown_value".to_string())
        );
    }

    #[test]
    fn build_messages_roundtrip() {
        let messages = vec![
            ChatMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text { text: "Hi".into() }],
            },
            ChatMessage {
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Sure".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "run".into(),
                        input: json!({"cmd": "ls"}),
                    },
                ],
            },
            ChatMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file.txt".into(),
                    is_error: false,
                }],
            },
        ];

        let api_msgs = AnthropicProvider::build_messages(&messages);
        let serialized = serde_json::to_value(&api_msgs).unwrap();
        let arr = serialized.as_array().unwrap();

        assert_eq!(arr.len(), 3);

        // User text message
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"][0]["type"], "text");
        assert_eq!(arr[0]["content"][0]["text"], "Hi");

        // Assistant with text + tool_use
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[1]["content"][0]["type"], "text");
        assert_eq!(arr[1]["content"][0]["text"], "Sure");
        assert_eq!(arr[1]["content"][1]["type"], "tool_use");
        assert_eq!(arr[1]["content"][1]["id"], "t1");
        assert_eq!(arr[1]["content"][1]["name"], "run");
        assert_eq!(arr[1]["content"][1]["input"]["cmd"], "ls");

        // User with tool_result
        assert_eq!(arr[2]["role"], "user");
        assert_eq!(arr[2]["content"][0]["type"], "tool_result");
        assert_eq!(arr[2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(arr[2]["content"][0]["content"], "file.txt");
        assert_eq!(arr[2]["content"][0]["is_error"], false);
    }

    #[test]
    fn build_tools_produces_correct_json() {
        let schemas = vec![
            ToolSchema {
                name: "shell_exec".into(),
                description: "Run a shell command".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    },
                    "required": ["command"]
                }),
            },
            ToolSchema {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    },
                    "required": ["path"]
                }),
            },
        ];

        let api_tools = AnthropicProvider::build_tools(&schemas, false);
        let serialized = serde_json::to_value(&api_tools).unwrap();
        let arr = serialized.as_array().unwrap();

        assert_eq!(arr.len(), 2);

        assert_eq!(arr[0]["name"], "shell_exec");
        assert_eq!(arr[0]["description"], "Run a shell command");
        assert_eq!(arr[0]["input_schema"]["type"], "object");
        assert_eq!(arr[0]["input_schema"]["required"][0], "command");

        assert_eq!(arr[1]["name"], "read_file");
        assert_eq!(arr[1]["description"], "Read a file");
        assert_eq!(arr[1]["input_schema"]["required"][0], "path");
    }
}
