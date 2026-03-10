use std::sync::Arc;
use std::time::{Duration, Instant};
use futures::future::join_all;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use apex_core::context::TokenEstimator;
use apex_core::domain::{
    ChatMessage, CompletionRequest, ContentBlock, LogEntry, MessageRole,
    ToolCallRecord, TurnRecord,
};
use apex_core::ports::{LlmProvider, ToolRegistry, WorkingMemory};

use apex_core::summarize_json;

use crate::compaction::compact_messages;

use crate::constants::{COMPACT_CONVERSATION_TOOL, MAX_TURNS};

/// Configuration bundle for the agentic loop.
pub struct LoopConfig<'a> {
    pub persona: &'a str,
    pub llm: &'a dyn LlmProvider,
    pub tools: &'a dyn ToolRegistry,
    pub estimator: &'a Arc<Mutex<TokenEstimator>>,
    pub max_tool_result_bytes: usize,
    pub max_output_tokens: u32,
    pub scratchpad: Option<&'a Arc<Mutex<apex_core::domain::Scratchpad>>>,
    pub memory: Option<&'a dyn WorkingMemory>,
    /// Optional cancellation token — checked before each turn.
    pub cancel: Option<&'a CancellationToken>,
    /// Optional wall-clock timeout for the entire loop.
    pub timeout: Option<Duration>,
    pub compaction_preserve_turns: usize,
    pub compaction_max_summary_tokens: u32,
}

/// Runs the multi-turn LLM + tool execution loop. Shared between
/// execute_task, execute_continuation, and the `delegate` tool (sub-agents).
pub async fn run_agentic_loop(
    initial_messages: Vec<ChatMessage>,
    config: &LoopConfig<'_>,
) -> (Vec<TurnRecord>, Option<String>, Vec<ChatMessage>) {
    let mut messages = initial_messages;
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut final_text: Option<String> = None;
    let deadline = config.timeout.map(|d| Instant::now() + d);
    let schemas = config.tools.schemas();
    let system_prompt = config.persona.to_string();

    for turn_num in 0..MAX_TURNS {
        // Check cancellation and timeout before each turn
        if let Some(token) = config.cancel {
            if token.is_cancelled() {
                final_text = Some("Cancelled".to_string());
                break;
            }
        }
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                final_text = Some("LLM error: loop timeout exceeded".to_string());
                break;
            }
        }

        let req = CompletionRequest {
            system_prompt: &system_prompt,
            messages: &messages,
            max_tokens: config.max_output_tokens,
            temperature: Some(0.2),
        };

        let resp = match config.llm.complete_with_tools(req, &schemas).await {
            Ok(r) => r,
            Err(err) => {
                // Signal LLM error via final_text starting with "LLM error:"
                final_text = Some(format!("LLM error: {err}"));
                break;
            }
        };

        eprintln!(
            "  turn {}: {} tool call(s), {} input / {} output tokens",
            turn_num + 1,
            resp.tool_calls.len(),
            resp.usage.input_tokens,
            resp.usage.output_tokens,
        );

        // Calibrate token estimator (in-memory only; caller persists after loop)
        {
            let prompt_text: String = messages
                .iter()
                .map(|m| m.text())
                .collect::<Vec<_>>()
                .join("\n");
            let mut est = config.estimator.lock().await;
            est.calibrate(&prompt_text, resp.usage.input_tokens);
        }

        messages.push(resp.message.clone());

        if resp.tool_calls.is_empty() {
            let text = resp.text();
            if !text.is_empty() {
                final_text = Some(text);
            }
            turns.push(TurnRecord {
                tool_calls: vec![],
                usage: resp.usage,
            });
            break;
        }

        let mut call_records = Vec::new();
        let mut result_blocks = Vec::new();

        // Execute non-compaction tool calls concurrently
        let tool_futures: Vec<_> = resp.tool_calls.iter()
            .filter(|c| c.name != COMPACT_CONVERSATION_TOOL)
            .map(|call| {
            async move {
                eprintln!("  ↳ {}(…)", call.name);
                let start = Instant::now();
                let result = match config.tools.execute(call).await {
                    Ok(r) => r,
                    Err(err) => apex_core::domain::ToolResult {
                        tool_use_id: call.id.clone(),
                        name: call.name.clone(),
                        output: serde_json::json!({ "error": err.to_string() }),
                        is_error: true,
                        ..Default::default()
                    },
                };
                (call, result, start.elapsed())
            }
        }).collect();

        let results = join_all(tool_futures).await;

        for (call, result, elapsed) in results {
            let duration_ms = elapsed.as_millis() as u64;

            call_records.push(ToolCallRecord {
                name: call.name.clone(),
                input_summary: summarize_json(&call.input, 80),
                output_summary: summarize_json(&result.output, 120),
                is_error: result.is_error,
                duration_ms,
            });

            let raw_content = serde_json::to_string(&result.output)
                .unwrap_or_else(|_| "{}".to_string());
            let content = if raw_content.len() > config.max_tool_result_bytes {
                let truncated = apex_core::truncate_str(&raw_content, config.max_tool_result_bytes);
                format!("{truncated}\n\n[truncated: {orig} bytes → {kept} bytes]",
                    orig = raw_content.len(), kept = truncated.len())
            } else {
                raw_content
            };
            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: result.tool_use_id,
                content,
                is_error: result.is_error,
            });
        }

        // Handle compact_conversation (take first, reject duplicates)
        {
            let mut compact_iter = resp.tool_calls.iter()
                .filter(|c| c.name == COMPACT_CONVERSATION_TOOL);
            if let Some(compact_call) = compact_iter.next() {
                eprintln!("  ↳ compact_conversation(…)");
                let start = Instant::now();
                match compact_messages(
                    &messages,
                    config.llm,
                    config.compaction_preserve_turns,
                    config.compaction_max_summary_tokens,
                ).await {
                    Ok((compacted, count)) => {
                        messages = compacted;
                        let elapsed = start.elapsed();
                        call_records.push(ToolCallRecord {
                            name: COMPACT_CONVERSATION_TOOL.into(),
                            input_summary: "{}".into(),
                            output_summary: format!("compacted {count} messages"),
                            is_error: false,
                            duration_ms: elapsed.as_millis() as u64,
                        });
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: compact_call.id.clone(),
                            content: format!("Compacted {count} older messages into a summary. Recent turns preserved."),
                            is_error: false,
                        });
                    }
                    Err(reason) => {
                        let elapsed = start.elapsed();
                        call_records.push(ToolCallRecord {
                            name: COMPACT_CONVERSATION_TOOL.into(),
                            input_summary: "{}".into(),
                            output_summary: format!("failed: {reason}"),
                            is_error: true,
                            duration_ms: elapsed.as_millis() as u64,
                        });
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: compact_call.id.clone(),
                            content: format!("Compaction failed: {reason}"),
                            is_error: true,
                        });
                    }
                }
                // Error result for duplicate calls
                for dup in compact_iter {
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: dup.id.clone(),
                        content: "Already compacted this turn".into(),
                        is_error: true,
                    });
                }
            }
        }

        // Persist log entries to scratchpad after each turn
        if let (Some(pad_mutex), Some(mem)) = (config.scratchpad, config.memory) {
            let mut pad = pad_mutex.lock().await;
            for tc in &call_records {
                pad.log.push(LogEntry {
                    turn: (turn_num + 1) as u32,
                    tool_name: tc.name.clone(),
                    input_summary: tc.input_summary.clone(),
                    output_summary: tc.output_summary.clone(),
                    is_error: tc.is_error,
                    duration_ms: tc.duration_ms,
                });
            }
            let _ = mem.save(&pad).await;
        }

        turns.push(TurnRecord {
            tool_calls: call_records,
            usage: resp.usage,
        });

        messages.push(ChatMessage {
            role: MessageRole::User,
            content: result_blocks,
        });
    }

    (turns, final_text, messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::ToolCall;
    use crate::test_mocks::*;

    fn default_estimator() -> Arc<Mutex<TokenEstimator>> {
        Arc::new(Mutex::new(TokenEstimator::default()))
    }

    #[tokio::test]
    async fn loop_returns_on_end_turn() {
        let llm = MockLlmProvider::text_only("Hello, world!");
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();

        let config = LoopConfig {
            persona: "You are helpful.",
            llm: &llm,
            tools: &tools,
            estimator: &estimator,
            max_tool_result_bytes: 10_000,
            max_output_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: None,
            timeout: None,
            compaction_preserve_turns: 3,
            compaction_max_summary_tokens: 1024,
        };

        let messages = vec![ChatMessage::user_text("Hi")];
        let (turns, final_text, _msgs) = run_agentic_loop(messages, &config).await;

        assert_eq!(turns.len(), 1);
        assert!(turns[0].tool_calls.is_empty());
        assert_eq!(final_text.as_deref(), Some("Hello, world!"));
    }

    #[tokio::test]
    async fn loop_executes_tool_and_continues() {
        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "test_tool".into(),
            input: serde_json::json!({}),
        };
        let llm = MockLlmProvider::tool_then_text(tool_call, "Done!");
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();

        let config = LoopConfig {
            persona: "You are helpful.",
            llm: &llm,
            tools: &tools,
            estimator: &estimator,
            max_tool_result_bytes: 10_000,
            max_output_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: None,
            timeout: None,
            compaction_preserve_turns: 3,
            compaction_max_summary_tokens: 1024,
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, final_text, _msgs) = run_agentic_loop(messages, &config).await;

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].tool_calls.len(), 1);
        assert_eq!(turns[0].tool_calls[0].name, "test_tool");
        assert!(turns[1].tool_calls.is_empty());
        assert_eq!(final_text.as_deref(), Some("Done!"));

        // Verify the tool was actually called
        let calls = tools.calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test_tool");
    }

    #[tokio::test]
    async fn loop_handles_llm_error() {
        let llm = MockLlmProvider::error("API overloaded");
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();

        let config = LoopConfig {
            persona: "You are helpful.",
            llm: &llm,
            tools: &tools,
            estimator: &estimator,
            max_tool_result_bytes: 10_000,
            max_output_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: None,
            timeout: None,
            compaction_preserve_turns: 3,
            compaction_max_summary_tokens: 1024,
        };

        let messages = vec![ChatMessage::user_text("Hi")];
        let (_turns, final_text, _msgs) = run_agentic_loop(messages, &config).await;

        let text = final_text.unwrap();
        assert!(text.starts_with("LLM error:"), "got: {text}");
        assert!(text.contains("API overloaded"));
    }

    #[tokio::test]
    async fn loop_converts_tool_error() {
        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "bad_tool".into(),
            input: serde_json::json!({}),
        };
        let llm = MockLlmProvider::tool_then_text(tool_call, "Recovered");
        let tools = MockToolRegistry::failing("bad_tool", "disk full");
        let estimator = default_estimator();

        let config = LoopConfig {
            persona: "You are helpful.",
            llm: &llm,
            tools: &tools,
            estimator: &estimator,
            max_tool_result_bytes: 10_000,
            max_output_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: None,
            timeout: None,
            compaction_preserve_turns: 3,
            compaction_max_summary_tokens: 1024,
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, final_text, _msgs) = run_agentic_loop(messages, &config).await;

        // Tool error should be converted to ToolResult with is_error:true
        assert_eq!(turns[0].tool_calls.len(), 1);
        assert!(turns[0].tool_calls[0].is_error);
        // Loop should continue and get the final text
        assert_eq!(final_text.as_deref(), Some("Recovered"));
    }

    #[tokio::test]
    async fn loop_respects_cancellation() {
        let llm = MockLlmProvider::text_only("Should not reach");
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();
        let cancel = CancellationToken::new();
        cancel.cancel(); // Pre-cancel

        let config = LoopConfig {
            persona: "You are helpful.",
            llm: &llm,
            tools: &tools,
            estimator: &estimator,
            max_tool_result_bytes: 10_000,
            max_output_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: Some(&cancel),
            timeout: None,
            compaction_preserve_turns: 3,
            compaction_max_summary_tokens: 1024,
        };

        let messages = vec![ChatMessage::user_text("Hi")];
        let (turns, final_text, _msgs) = run_agentic_loop(messages, &config).await;

        assert!(turns.is_empty());
        assert_eq!(final_text.as_deref(), Some("Cancelled"));
    }

    #[tokio::test]
    async fn loop_respects_timeout() {
        let llm = MockLlmProvider::text_only("Should not reach");
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();

        let config = LoopConfig {
            persona: "You are helpful.",
            llm: &llm,
            tools: &tools,
            estimator: &estimator,
            max_tool_result_bytes: 10_000,
            max_output_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: None,
            timeout: Some(Duration::from_secs(0)), // Already expired
            compaction_preserve_turns: 3,
            compaction_max_summary_tokens: 1024,
        };

        let messages = vec![ChatMessage::user_text("Hi")];
        let (turns, final_text, _msgs) = run_agentic_loop(messages, &config).await;

        assert!(turns.is_empty());
        let text = final_text.unwrap();
        assert!(text.contains("loop timeout exceeded"), "got: {text}");
    }

    #[tokio::test]
    async fn loop_stops_at_max_turns() {
        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "test_tool".into(),
            input: serde_json::json!({}),
        };
        // Queue MAX_TURNS worth of tool-call responses
        let llm = MockLlmProvider::always_tool_call(tool_call, MAX_TURNS + 5);
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();

        let config = LoopConfig {
            persona: "You are helpful.",
            llm: &llm,
            tools: &tools,
            estimator: &estimator,
            max_tool_result_bytes: 10_000,
            max_output_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: None,
            timeout: None,
            compaction_preserve_turns: 3,
            compaction_max_summary_tokens: 1024,
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, _final_text, _msgs) = run_agentic_loop(messages, &config).await;

        assert_eq!(turns.len(), MAX_TURNS);
    }

    #[tokio::test]
    async fn loop_truncates_large_tool_output() {
        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "big_tool".into(),
            input: serde_json::json!({}),
        };
        let llm = MockLlmProvider::tool_then_text(tool_call, "Done");
        let tools = MockToolRegistry::large_output("big_tool", 100_000);
        let estimator = default_estimator();

        let max_result_bytes = 1_000;
        let config = LoopConfig {
            persona: "You are helpful.",
            llm: &llm,
            tools: &tools,
            estimator: &estimator,
            max_tool_result_bytes: max_result_bytes,
            max_output_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: None,
            timeout: None,
            compaction_preserve_turns: 3,
            compaction_max_summary_tokens: 1024,
        };

        let messages = vec![ChatMessage::user_text("Get data")];
        let (_turns, _final_text, msgs) = run_agentic_loop(messages, &config).await;

        // Find the tool result message and check it was truncated
        let tool_result_msg = msgs.iter().find(|m| {
            m.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        });
        assert!(tool_result_msg.is_some(), "should have a tool result message");

        let tool_result_content = tool_result_msg.unwrap().content.iter().find_map(|b| {
            if let ContentBlock::ToolResult { content, .. } = b {
                Some(content.clone())
            } else {
                None
            }
        }).unwrap();

        assert!(tool_result_content.contains("[truncated:"), "output should contain truncation marker, got len={}", tool_result_content.len());
    }
}
