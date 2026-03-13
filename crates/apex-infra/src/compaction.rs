use std::sync::Arc;

use async_trait::async_trait;

use apex_core::domain::{
    CacheHint, ChatMessage, CompletionRequest, ContentBlock, MessageRole, SystemBlock,
};
use apex_core::ports::{ConversationCompactor, LlmProvider};
use apex_core::truncate_str;

const MAX_SUMMARY_INPUT_CHARS: usize = 32_000;

const SUMMARIZATION_SYSTEM_PROMPT: &str = "\
You are a conversation summarizer for an AI coding assistant. \
Produce a concise summary of the conversation so far. \
Preserve: key decisions made, important tool outputs and their results, \
errors encountered and how they were resolved, file paths and code snippets referenced, \
and any facts or constraints discovered. \
Omit: redundant tool calls, verbose raw output, and conversational filler. \
Format the summary as a structured list with clear section headings.";

pub struct LlmConversationCompactor {
    llm: Arc<dyn LlmProvider>,
}

impl LlmConversationCompactor {
    pub fn new(llm: Arc<dyn LlmProvider>) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl ConversationCompactor for LlmConversationCompactor {
    async fn compact(
        &self,
        messages: &[ChatMessage],
        preserve_turns: usize,
        max_summary_tokens: u32,
    ) -> Result<(Vec<ChatMessage>, usize), String> {
        // Need at least 3 messages to compact (original + something + preserved)
        if messages.len() < 3 {
            return Err("too few messages to compact".into());
        }

        // Compute split point: preserve first message + last N turns (each turn ≈ 2 messages)
        let preserve_msgs = (preserve_turns * 2).min(messages.len().saturating_sub(1));
        let mut split_point = messages.len().saturating_sub(preserve_msgs);

        // Nothing to compact if split would keep everything
        if split_point <= 1 {
            return Err("nothing to compact after preserving recent turns".into());
        }

        // Ensure preserved tail starts with a User message (for alternation after Assistant summary)
        if split_point < messages.len() && messages[split_point].role == MessageRole::Assistant {
            if split_point + 1 < messages.len() {
                split_point += 1;
            } else {
                return Err("cannot maintain message alternation".into());
            }
        }

        // Serialize messages to summarize into a text prompt
        let mut conversation_text = String::new();
        for msg in &messages[1..split_point] {
            let role_label = match msg.role {
                MessageRole::Assistant => "Assistant",
                MessageRole::User => "User",
            };
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        conversation_text.push_str(&format!("[{role_label}]: {text}\n\n"));
                    }
                    ContentBlock::ToolUse { name, .. } => {
                        conversation_text
                            .push_str(&format!("[{role_label}]: Called tool: {name}\n\n"));
                    }
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        let label = if *is_error { "error" } else { "ok" };
                        conversation_text
                            .push_str(&format!("[Tool result ({label})]: {content}\n\n"));
                    }
                }
            }
        }

        let truncated = truncate_str(&conversation_text, MAX_SUMMARY_INPUT_CHARS);
        let compacted_count = split_point - 1;

        let prompt = format!(
            "Summarize the following conversation excerpt ({compacted_count} messages). \
             The original task was: {}\n\n---\n\n{truncated}",
            messages[0].text(),
        );

        let summary_messages = vec![ChatMessage::user_text(&prompt)];
        let system_blocks = [SystemBlock {
            text: SUMMARIZATION_SYSTEM_PROMPT.to_string(),
            cache_hint: CacheHint::Dynamic,
        }];
        let req = CompletionRequest {
            system_blocks: &system_blocks,
            messages: &summary_messages,
            max_tokens: max_summary_tokens,
            temperature: Some(0.0),
            cache_tools: false,
            reserved_reasoning_tokens: 0,
        };

        let summary_text = match self.llm.complete(req).await {
            Ok(resp) => {
                let text = resp.text();
                if text.is_empty() {
                    return Err("LLM returned empty summary".into());
                }
                format!("[Conversation compacted: {compacted_count} messages summarized]\n\n{text}")
            }
            Err(err) => {
                return Err(format!("LLM summary failed: {err}"));
            }
        };

        let preserve_count = messages.len() - split_point;
        let mut result = Vec::with_capacity(1 + 1 + preserve_count);
        result.push(messages[0].clone()); // original task (User)
        result.push(ChatMessage {
            // summary (Assistant)
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text { text: summary_text }],
        });
        result.extend_from_slice(&messages[split_point..]); // User, Assistant, User, ...

        Ok((result, compacted_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::{CompletionResponse, StopReason, TokenUsage};
    use apex_core::error::LlmError;
    use async_trait::async_trait;

    // ── Mock LLM provider ───────────────────────────────────────────

    struct MockLlmProvider {
        response: Result<String, LlmError>,
    }

    impl MockLlmProvider {
        fn success(text: &str) -> Self {
            Self {
                response: Ok(text.to_string()),
            }
        }

        fn error() -> Self {
            Self {
                response: Err(LlmError::Api("mock error".into())),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockLlmProvider {
        async fn complete(
            &self,
            _req: CompletionRequest<'_>,
        ) -> Result<CompletionResponse, LlmError> {
            match &self.response {
                Ok(text) => Ok(CompletionResponse {
                    message: ChatMessage {
                        role: MessageRole::Assistant,
                        content: vec![ContentBlock::Text { text: text.clone() }],
                    },
                    usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                        ..Default::default()
                    },
                    stop_reason: StopReason::EndTurn,
                }),
                Err(e) => Err(LlmError::Api(e.to_string())),
            }
        }

        async fn complete_with_tools(
            &self,
            _req: CompletionRequest<'_>,
            _tools: &[apex_core::domain::ToolSchema],
        ) -> Result<apex_core::domain::ToolCompletionResponse, LlmError> {
            unimplemented!("not needed for these tests")
        }

        fn model_id(&self) -> &str {
            "mock-model"
        }
        fn context_window(&self) -> usize {
            200_000
        }
    }

    // ── Helper: build messages ──────────────────────────────────────

    fn make_messages(pairs: usize) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user_text("Original task")];
        for i in 0..pairs {
            messages.push(ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("Response {i}"),
                }],
            });
            messages.push(ChatMessage::user_text(format!("Follow-up {i}")));
        }
        messages
    }

    // ── Compactor tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn compact_uses_llm_summary() {
        let llm = Arc::new(MockLlmProvider::success(
            "## Summary\n- Worked on the original task\n- Used shell_exec to run tests",
        ));
        let compactor = LlmConversationCompactor::new(llm);
        let messages = make_messages(20);

        let result = compactor.compact(&messages, 3, 1024).await;

        assert!(result.is_ok());
        let (compacted, count) = result.unwrap();
        assert!(count > 0);
        assert_eq!(compacted[0].text(), "Original task");
        let summary = compacted[1].text();
        assert!(summary.contains("compacted"));
        assert!(summary.contains("Worked on the original task"));
        assert!(compacted.len() < messages.len());
        assert_eq!(compacted[0].role, MessageRole::User);
        assert_eq!(compacted[1].role, MessageRole::Assistant);
        for i in 1..compacted.len() {
            assert_ne!(
                compacted[i].role,
                compacted[i - 1].role,
                "alternation violated at index {i}"
            );
        }
    }

    #[tokio::test]
    async fn compact_returns_error_on_llm_failure() {
        let llm = Arc::new(MockLlmProvider::error());
        let compactor = LlmConversationCompactor::new(llm);
        let messages = make_messages(20);

        let result = compactor.compact(&messages, 3, 1024).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("LLM summary failed"));
    }

    #[tokio::test]
    async fn compact_returns_error_when_too_few_messages() {
        let llm = Arc::new(MockLlmProvider::success("summary"));
        let compactor = LlmConversationCompactor::new(llm);
        let messages = vec![
            ChatMessage::user_text("Do something"),
            ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text { text: "OK".into() }],
            },
        ];

        let result = compactor.compact(&messages, 6, 1024).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too few messages"));
    }

    #[tokio::test]
    async fn compact_split_point_starts_with_user() {
        let llm = Arc::new(MockLlmProvider::success("Summary text"));
        let compactor = LlmConversationCompactor::new(llm);
        let messages = make_messages(10);

        let result = compactor.compact(&messages, 2, 1024).await;

        assert!(result.is_ok());
        let (compacted, _) = result.unwrap();
        for i in 1..compacted.len() {
            assert_ne!(
                compacted[i].role,
                compacted[i - 1].role,
                "alternation violated at index {i}"
            );
        }
    }

    #[tokio::test]
    async fn characterization_compaction_preserves_first_message_verbatim() {
        let llm = Arc::new(MockLlmProvider::success("Summary of work"));
        let compactor = LlmConversationCompactor::new(llm);
        let messages = make_messages(10);

        let (compacted, _count) = compactor
            .compact(&messages, 2, 1024)
            .await
            .expect("compaction should succeed");

        assert_eq!(compacted[0].role, MessageRole::User);
        assert_eq!(
            compacted[0].text(),
            "Original task",
            "first message must be preserved exactly as the original task"
        );
    }

    #[tokio::test]
    async fn characterization_compaction_summary_contains_count_marker() {
        let llm = Arc::new(MockLlmProvider::success("Summary of conversation"));
        let compactor = LlmConversationCompactor::new(llm);
        let messages = make_messages(10);

        let (compacted, count) = compactor
            .compact(&messages, 2, 1024)
            .await
            .expect("compaction should succeed");

        assert!(count > 0, "should report non-zero compacted count");
        let summary_text = compacted[1].text();
        assert!(
            summary_text.contains(&format!("{count} messages summarized")),
            "summary should contain the compacted count marker, got: {summary_text}"
        );
        assert_eq!(
            compacted[1].role,
            MessageRole::Assistant,
            "summary must be an Assistant message for proper alternation"
        );
    }

    #[tokio::test]
    async fn characterization_compaction_preserved_tail_is_unchanged() {
        let llm = Arc::new(MockLlmProvider::success("Summary"));
        let compactor = LlmConversationCompactor::new(llm);
        let messages = make_messages(8);
        let preserve_turns = 2;

        let (compacted, count) = compactor
            .compact(&messages, preserve_turns, 1024)
            .await
            .expect("compaction should succeed");

        let actual_split = count + 1;
        let original_tail = &messages[actual_split..];
        let compacted_tail = &compacted[2..];

        assert_eq!(
            compacted_tail.len(),
            original_tail.len(),
            "preserved tail length must match: expected {} got {}",
            original_tail.len(),
            compacted_tail.len()
        );

        for (i, (orig, comp)) in original_tail.iter().zip(compacted_tail.iter()).enumerate() {
            assert_eq!(orig.role, comp.role, "tail message {i} role mismatch");
            assert_eq!(orig.text(), comp.text(), "tail message {i} text mismatch");
        }
    }

    #[tokio::test]
    async fn characterization_compaction_count_equals_summarized_middle() {
        let llm = Arc::new(MockLlmProvider::success("Summary"));
        let compactor = LlmConversationCompactor::new(llm);
        let messages = make_messages(10);
        let preserve_turns = 3;

        let (compacted, count) = compactor
            .compact(&messages, preserve_turns, 1024)
            .await
            .expect("compaction should succeed");

        let preserved_tail_len = compacted.len() - 2;
        let expected_count = messages.len() - 1 - preserved_tail_len;
        assert_eq!(
            count, expected_count,
            "returned count must equal the number of middle messages that were summarized"
        );
    }

    #[tokio::test]
    async fn characterization_compaction_with_tool_use_blocks() {
        let llm = Arc::new(MockLlmProvider::success("Ran cargo test, 3 tests passed"));
        let compactor = LlmConversationCompactor::new(llm);

        let mut messages = vec![ChatMessage::user_text("Fix the bug")];
        messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu-1".into(),
                name: "shell_exec".into(),
                input: serde_json::json!({"cmd": "cargo test"}),
            }],
        });
        messages.push(ChatMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu-1".into(),
                content: "3 tests passed".into(),
                is_error: false,
            }],
        });
        for i in 0..6 {
            messages.push(ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("Resp {i}"),
                }],
            });
            messages.push(ChatMessage::user_text(format!("Follow {i}")));
        }

        let result = compactor.compact(&messages, 2, 1024).await;

        assert!(
            result.is_ok(),
            "compaction should handle ToolUse/ToolResult blocks"
        );
        let (compacted, count) = result.unwrap();
        assert!(count > 0);
        for i in 1..compacted.len() {
            assert_ne!(
                compacted[i].role,
                compacted[i - 1].role,
                "alternation violated at index {i}"
            );
        }
    }

    #[tokio::test]
    async fn compact_with_odd_message_count_adjusts_split() {
        let llm = Arc::new(MockLlmProvider::success("Summary"));
        let compactor = LlmConversationCompactor::new(llm);

        let mut messages = vec![ChatMessage::user_text("Task")];
        for i in 0..5 {
            messages.push(ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("Resp {i}"),
                }],
            });
            messages.push(ChatMessage::user_text(format!("Follow {i}")));
        }
        messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: "Final resp".into(),
            }],
        });

        let result = compactor.compact(&messages, 2, 1024).await;

        assert!(result.is_ok());
        let (compacted, _) = result.unwrap();
        for i in 1..compacted.len() {
            assert_ne!(
                compacted[i].role,
                compacted[i - 1].role,
                "alternation violated at index {i}"
            );
        }
    }
}
