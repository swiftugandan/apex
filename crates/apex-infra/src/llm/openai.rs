use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use apex_core::domain::{
    ChatMessage, CompletionRequest, CompletionResponse, ContentBlock, MessageRole,
    OutputTokensDetails, StopReason, SystemBlock, TokenUsage, ToolCall, ToolCompletionResponse,
    ToolSchema,
};
use apex_core::error::LlmError;
use apex_core::ports::LlmProvider;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

pub struct OpenAiProvider {
    client: Client,
    /// Pre-computed endpoint URL (base_url + "/chat/completions").
    endpoint_url: String,
    /// Pre-computed Authorization header value.
    auth_header: String,
    model: String,
    context_window: usize,
    /// Optional OpenRouter-specific headers, read once at construction.
    openrouter_referer: Option<String>,
    openrouter_title: Option<String>,
}

impl OpenAiProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        context_window: usize,
    ) -> Self {
        let base = base_url.into();
        let key = api_key.into();
        Self {
            client: Client::new(),
            endpoint_url: format!("{}/chat/completions", base.trim_end_matches('/')),
            auth_header: format!("Bearer {key}"),
            model: model.into(),
            context_window,
            openrouter_referer: std::env::var("OPENROUTER_HTTP_REFERER").ok(),
            openrouter_title: std::env::var("OPENROUTER_X_TITLE").ok(),
        }
    }

    /// Create from config values, reading the API key from the environment.
    pub fn from_env_with_config(model: &str, base_url: Option<&str>) -> Result<Self, LlmError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .map_err(|_| {
                LlmError::Configuration(
                    "OPENAI_API_KEY or OPENROUTER_API_KEY environment variable must be set".into(),
                )
            })?;
        let base_url = base_url
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .or_else(|| std::env::var("OPENROUTER_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let context_window = std::env::var("APEX_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        Ok(Self::new(api_key, base_url, model, context_window))
    }

    async fn send_request(&self, body: Value) -> Result<OaiResponse, LlmError> {
        let mut request = self
            .client
            .post(&self.endpoint_url)
            .header("Authorization", &self.auth_header)
            .header("content-type", "application/json");

        // OpenRouter-specific headers
        if let Some(ref referer) = self.openrouter_referer {
            request = request.header("HTTP-Referer", referer);
        }
        if let Some(ref title) = self.openrouter_title {
            request = request.header("X-Title", title);
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
            .json::<OaiResponse>()
            .await
            .map_err(|e| LlmError::Serialization(e.to_string()))
    }

    fn build_system_message(blocks: &[SystemBlock]) -> OaiMessage {
        let text = blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        OaiMessage {
            role: "system".into(),
            content: Some(text),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn build_messages(messages: &[ChatMessage]) -> Vec<OaiMessage> {
        let mut out = Vec::new();
        for msg in messages {
            match msg.role {
                MessageRole::Assistant => {
                    let mut text_parts: Vec<&str> = Vec::new();
                    let mut tool_calls = Vec::new();

                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => text_parts.push(text),
                            ContentBlock::ToolUse { id, name, input } => {
                                tool_calls.push(OaiToolCall {
                                    id: id.clone(),
                                    r#type: "function".into(),
                                    function: OaiFunctionCall {
                                        name: name.clone(),
                                        arguments: serde_json::to_string(input).unwrap_or_default(),
                                    },
                                });
                            }
                            ContentBlock::ToolResult { .. } => {}
                        }
                    }

                    // Some providers (e.g. StepFun) reject assistant messages with
                    // content: null when tool_calls is present. Always send at least
                    // an empty string to avoid "Unrecognized chat message" errors.
                    let content_str = if text_parts.is_empty() {
                        String::new()
                    } else {
                        text_parts.join("\n")
                    };

                    out.push(OaiMessage {
                        role: "assistant".into(),
                        content: Some(content_str),
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        tool_call_id: None,
                    });
                }
                MessageRole::User => {
                    let mut text_parts: Vec<&str> = Vec::new();

                    // Emit tool result messages first
                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } => text_parts.push(text),
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } => {
                                out.push(OaiMessage {
                                    role: "tool".into(),
                                    content: Some(content.clone()),
                                    tool_calls: None,
                                    tool_call_id: Some(tool_use_id.clone()),
                                });
                            }
                            ContentBlock::ToolUse { .. } => {}
                        }
                    }

                    // Emit user text if present
                    if !text_parts.is_empty() {
                        out.push(OaiMessage {
                            role: "user".into(),
                            content: Some(text_parts.join("\n")),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                }
            }
        }
        out
    }

    fn build_tools(tools: &[ToolSchema]) -> Vec<OaiToolDef> {
        tools
            .iter()
            .map(|t| OaiToolDef {
                r#type: "function".into(),
                function: OaiFunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect()
    }

    fn parse_response(response: &OaiResponse) -> Result<(ChatMessage, Vec<ToolCall>), LlmError> {
        let choice = response
            .choices
            .first()
            .ok_or_else(|| LlmError::UnexpectedResponse("no choices in response".into()))?;

        let mut content_blocks = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(ref text) = choice.message.content {
            if !text.is_empty() {
                content_blocks.push(ContentBlock::Text { text: text.clone() });
            }
        }

        if let Some(ref calls) = choice.message.tool_calls {
            for call in calls {
                let input: Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or(Value::Object(serde_json::Map::new()));
                content_blocks.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    input: input.clone(),
                });
                tool_calls.push(ToolCall {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    input,
                });
            }
        }

        let message = ChatMessage {
            role: MessageRole::Assistant,
            content: content_blocks,
        };

        Ok((message, tool_calls))
    }

    fn parse_stop_reason(reason: Option<&str>) -> StopReason {
        match reason {
            Some("stop") => StopReason::EndTurn,
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Unknown("content_filter".into()),
            Some(other) => StopReason::Unknown(other.to_string()),
            None => StopReason::Unknown("null".into()),
        }
    }

    fn parse_usage(response: &OaiResponse) -> TokenUsage {
        match &response.usage {
            Some(u) => {
                let output_tokens_details = u.completion_tokens_details.as_ref().and_then(|d| {
                    d.reasoning_tokens.map(|r| OutputTokensDetails {
                        reasoning_tokens: Some(r),
                    })
                });
                TokenUsage {
                    input_tokens: u.prompt_tokens,
                    output_tokens: u.completion_tokens,
                    output_tokens_details,
                    ..TokenUsage::default()
                }
            }
            None => TokenUsage::default(),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, req: CompletionRequest<'_>) -> Result<CompletionResponse, LlmError> {
        let mut messages = vec![Self::build_system_message(req.system_blocks)];
        messages.extend(Self::build_messages(req.messages));

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "messages": messages,
        });

        if req.reserved_reasoning_tokens > 0 {
            body["reasoning"] = serde_json::json!({ "enabled": true });
        }

        let response = self.send_request(body).await?;
        let (message, _) = Self::parse_response(&response)?;
        let stop_reason = Self::parse_stop_reason(
            response
                .choices
                .first()
                .and_then(|c| c.finish_reason.as_deref()),
        );

        Ok(CompletionResponse {
            message,
            usage: Self::parse_usage(&response),
            stop_reason,
        })
    }

    async fn complete_with_tools(
        &self,
        req: CompletionRequest<'_>,
        tools: &[ToolSchema],
    ) -> Result<ToolCompletionResponse, LlmError> {
        let mut messages = vec![Self::build_system_message(req.system_blocks)];
        messages.extend(Self::build_messages(req.messages));

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": req.max_tokens,
            "messages": messages,
            "tools": Self::build_tools(tools),
            "tool_choice": "auto",
        });

        if req.reserved_reasoning_tokens > 0 {
            body["reasoning"] = serde_json::json!({ "enabled": true });
        }

        let response = self.send_request(body).await?;
        let (message, tool_calls) = Self::parse_response(&response)?;
        let stop_reason = Self::parse_stop_reason(
            response
                .choices
                .first()
                .and_then(|c| c.finish_reason.as_deref()),
        );

        Ok(ToolCompletionResponse {
            message,
            tool_calls,
            usage: Self::parse_usage(&response),
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

// ── OpenAI-specific serde types (internal only) ──

#[derive(Debug, Serialize)]
struct OaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiToolCall {
    id: String,
    r#type: String,
    function: OaiFunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OaiToolDef {
    r#type: String,
    function: OaiFunctionDef,
}

#[derive(Debug, Serialize)]
struct OaiFunctionDef {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Debug, Deserialize)]
struct OaiChoice {
    message: OaiChoiceMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OaiChoiceMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OaiCompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OaiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[serde(default)]
    completion_tokens_details: Option<OaiCompletionTokensDetails>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::CacheHint;
    use serde_json::json;

    #[test]
    fn deserialize_text_response() {
        let raw = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hello world"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });

        let resp: OaiResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("Hello world")
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.usage.as_ref().unwrap().prompt_tokens, 10);
        assert_eq!(resp.usage.as_ref().unwrap().completion_tokens, 5);
    }

    #[test]
    fn deserialize_tool_call_response() {
        let raw = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "I'll run that.",
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "shell_exec",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 15}
        });

        let resp: OaiResponse = serde_json::from_value(raw).unwrap();
        let (msg, calls) = OpenAiProvider::parse_response(&resp).unwrap();

        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.content.len(), 2);
        match &msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "I'll run that."),
            other => panic!("expected Text, got {other:?}"),
        }
        match &msg.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "shell_exec");
                assert_eq!(input, &json!({"command": "ls"}));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_123");
        assert_eq!(calls[0].name, "shell_exec");
    }

    #[test]
    fn parse_stop_reason_all_variants() {
        assert_eq!(
            OpenAiProvider::parse_stop_reason(Some("stop")),
            StopReason::EndTurn
        );
        assert_eq!(
            OpenAiProvider::parse_stop_reason(Some("tool_calls")),
            StopReason::ToolUse
        );
        assert_eq!(
            OpenAiProvider::parse_stop_reason(Some("length")),
            StopReason::MaxTokens
        );
        assert_eq!(
            OpenAiProvider::parse_stop_reason(Some("unknown_val")),
            StopReason::Unknown("unknown_val".to_string())
        );
        assert_eq!(
            OpenAiProvider::parse_stop_reason(None),
            StopReason::Unknown("null".into())
        );
    }

    #[test]
    fn build_system_message_concatenates() {
        let blocks = vec![
            SystemBlock {
                text: "You are helpful.".into(),
                cache_hint: CacheHint::Static,
            },
            SystemBlock {
                text: "Be concise.".into(),
                cache_hint: CacheHint::Dynamic,
            },
        ];

        let msg = OpenAiProvider::build_system_message(&blocks);
        assert_eq!(msg.role, "system");
        assert_eq!(
            msg.content.as_deref(),
            Some("You are helpful.\n\nBe concise.")
        );
    }

    #[test]
    fn build_messages_user_and_assistant() {
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

        let oai_msgs = OpenAiProvider::build_messages(&messages);
        let serialized = serde_json::to_value(&oai_msgs).unwrap();
        let arr = serialized.as_array().unwrap();

        // User text
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "Hi");

        // Assistant with tool_call
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[1]["content"], "Sure");
        assert_eq!(arr[1]["tool_calls"][0]["id"], "t1");
        assert_eq!(arr[1]["tool_calls"][0]["type"], "function");
        assert_eq!(arr[1]["tool_calls"][0]["function"]["name"], "run");

        // Tool result
        assert_eq!(arr[2]["role"], "tool");
        assert_eq!(arr[2]["tool_call_id"], "t1");
        assert_eq!(arr[2]["content"], "file.txt");
    }

    #[test]
    fn build_tools_wraps_in_function() {
        let schemas = vec![ToolSchema {
            name: "shell_exec".into(),
            description: "Run a command".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }),
        }];

        let tools = OpenAiProvider::build_tools(&schemas);
        let serialized = serde_json::to_value(&tools).unwrap();
        let arr = serialized.as_array().unwrap();

        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "shell_exec");
        assert_eq!(arr[0]["function"]["description"], "Run a command");
        assert_eq!(arr[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn parse_usage_with_data() {
        let resp = OaiResponse {
            choices: vec![],
            usage: Some(OaiUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                completion_tokens_details: None,
            }),
        };

        let usage = OpenAiProvider::parse_usage(&resp);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
    }

    #[test]
    fn parse_usage_without_data() {
        let resp = OaiResponse {
            choices: vec![],
            usage: None,
        };

        let usage = OpenAiProvider::parse_usage(&resp);
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn parse_malformed_arguments_defaults_to_empty_object() {
        let raw = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_bad",
                        "type": "function",
                        "function": {
                            "name": "test",
                            "arguments": "not valid json"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });

        let resp: OaiResponse = serde_json::from_value(raw).unwrap();
        let (_, calls) = OpenAiProvider::parse_response(&resp).unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input, json!({}));
    }
}
