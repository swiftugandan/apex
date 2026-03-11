use std::sync::Arc;
use std::time::{Duration, Instant};
use futures::future::join_all;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use apex_core::context::TokenEstimator;
use apex_core::domain::{
    ChatMessage, CompletionRequest, ContentBlock, HookEvent, HookOutcome,
    LogEntry, MessageRole, ToolCallRecord, TurnRecord,
};
use apex_core::ports::{HookRegistry, LlmProvider, ToolRegistry, WorkingMemory};

use apex_core::summarize_json;

use crate::compaction::compact_messages;
use crate::constants::COMPACT_CONVERSATION_TOOL;
use crate::log::dispatch_log;

/// Outcome of an agentic loop run.
#[derive(Debug, Clone)]
pub enum LoopOutcome {
    /// Normal finish with optional final text from the LLM.
    Completed(Option<String>),
    /// LLM provider returned an error.
    LlmError(String),
    /// Cancellation token was triggered.
    Cancelled,
    /// Wall-clock deadline exceeded.
    TimedOut,
    /// A BeforeTurn hook blocked execution.
    BlockedByHook(String),
    /// Hit the turn limit without the LLM finishing.
    MaxTurnsExhausted,
}

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
    /// Maximum number of LLM turns in this loop.
    pub max_turns: usize,
    /// Optional lifecycle hook registry.
    pub hooks: Option<&'a dyn HookRegistry>,
}

/// Runs the multi-turn LLM + tool execution loop. Shared between
/// execute_task, execute_continuation, and the `delegate` tool (sub-agents).
pub async fn run_agentic_loop(
    initial_messages: Vec<ChatMessage>,
    config: &LoopConfig<'_>,
) -> (Vec<TurnRecord>, LoopOutcome, Vec<ChatMessage>) {
    let mut messages = initial_messages;
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut outcome: Option<LoopOutcome> = None;
    let deadline = config.timeout.map(|d| Instant::now() + d);
    let schemas = config.tools.schemas();
    let system_prompt = config.persona.to_string();

    for turn_num in 0..config.max_turns {
        // Check cancellation and timeout before each turn
        if let Some(token) = config.cancel {
            if token.is_cancelled() {
                outcome = Some(LoopOutcome::Cancelled);
                break;
            }
        }
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                outcome = Some(LoopOutcome::TimedOut);
                break;
            }
        }

        // ── before_turn hooks ──
        if let Some(hooks) = config.hooks {
            let ctx = serde_json::json!({ "turn": turn_num + 1 });
            let outcomes = hooks.dispatch(HookEvent::BeforeTurn, &ctx).await;
            for hook_outcome in outcomes {
                match hook_outcome {
                    HookOutcome::Inject(content) => {
                        // Inject content as a user message before the LLM call
                        messages.push(ChatMessage::user_text(&content));
                    }
                    HookOutcome::Block(msg) => {
                        outcome = Some(LoopOutcome::BlockedByHook(msg));
                        break;
                    }
                    HookOutcome::Continue(_) => {}
                }
            }
            if outcome.is_some() {
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
                outcome = Some(LoopOutcome::LlmError(format!("{err}")));
                break;
            }
        };

        {
            let msg = format!(
                "  turn {}: {} tool call(s), {} input / {} output tokens",
                turn_num + 1,
                resp.tool_calls.len(),
                resp.usage.input_tokens,
                resp.usage.output_tokens,
            );
            let tool_count = resp.tool_calls.len();
            let input_toks = resp.usage.input_tokens;
            let output_toks = resp.usage.output_tokens;
            dispatch_log(
                config.hooks,
                || serde_json::json!({
                    "level": "info",
                    "event": "turn_summary",
                    "turn": turn_num + 1,
                    "tool_calls": tool_count,
                    "input_tokens": input_toks,
                    "output_tokens": output_toks,
                }),
                &msg,
            )
            .await;
        }

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
            let final_text = if text.is_empty() { None } else { Some(text) };
            turns.push(TurnRecord {
                tool_calls: vec![],
                usage: resp.usage,
            });
            outcome = Some(LoopOutcome::Completed(final_text));
            break;
        }

        let mut call_records = Vec::new();
        let mut result_blocks = Vec::new();

        // Execute non-compaction tool calls concurrently
        // Note: before_tool_call hooks run per-call before execution.
        // If hooks need to block, we collect blocked calls first.
        let non_compact_calls: Vec<_> = resp.tool_calls.iter()
            .filter(|c| c.name != COMPACT_CONVERSATION_TOOL)
            .collect();

        // Check before_tool_call hooks (must be sequential since hooks may block)
        let mut blocked_calls: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(hooks) = config.hooks {
            for call in &non_compact_calls {
                let ctx = serde_json::json!({
                    "tool": call.name,
                    "name": call.name,
                    "id": call.id,
                    "input": call.input,
                });
                let outcomes = hooks.dispatch(HookEvent::BeforeToolCall, &ctx).await;
                for outcome in &outcomes {
                    if let HookOutcome::Block(msg) = outcome {
                        dispatch_log(
                            config.hooks,
                            || serde_json::json!({
                                "level": "warn",
                                "event": "tool_blocked",
                                "tool": &call.name,
                                "id": &call.id,
                                "reason": &msg,
                            }),
                            &format!("  ↳ {}(…) BLOCKED: {}", call.name, msg),
                        ).await;
                        blocked_calls.insert(call.id.clone());
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: call.id.clone(),
                            content: format!("Blocked by hook: {msg}"),
                            is_error: true,
                        });
                        call_records.push(ToolCallRecord {
                            name: call.name.clone(),
                            input_summary: summarize_json(&call.input, 80),
                            output_summary: format!("BLOCKED: {msg}"),
                            is_error: true,
                            duration_ms: 0,
                        });
                        break;
                    }
                }
            }
        }

        let tool_futures: Vec<_> = non_compact_calls.iter()
            .filter(|c| !blocked_calls.contains(&c.id))
            .map(|call| {
            async move {
                dispatch_log(
                    config.hooks,
                    || serde_json::json!({
                        "level": "info",
                        "event": "tool_start",
                        "tool": &call.name,
                        "id": &call.id,
                    }),
                    &format!("  ↳ {}(…)", call.name),
                ).await;
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

            // ── after_tool_result hooks ──
            // Pass untruncated content to hooks so they see the full output.
            let mut final_content = raw_content.clone();
            let mut was_transformed = false;
            if let Some(hooks) = config.hooks {
                let ctx = serde_json::json!({
                    "tool": call.name,
                    "name": call.name,
                    "id": call.id,
                    "output": &raw_content,
                    "is_error": result.is_error,
                    "max_tool_result_bytes": config.max_tool_result_bytes,
                });
                let outcomes = hooks.dispatch(HookEvent::AfterToolResult, &ctx).await;
                for outcome in outcomes {
                    if let HookOutcome::Continue(Some(transformed)) = outcome {
                        final_content = transformed;
                        was_transformed = true;
                    }
                }
            }

            // Apply default truncation as fallback only if no Transform hook acted.
            if !was_transformed && final_content.len() > config.max_tool_result_bytes {
                let truncated = apex_core::truncate_str(&final_content, config.max_tool_result_bytes);
                final_content = format!("{truncated}\n\n[truncated: {orig} bytes → {kept} bytes]",
                    orig = raw_content.len(), kept = truncated.len());
            }

            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: result.tool_use_id,
                content: final_content,
                is_error: result.is_error,
            });
        }

        // Handle compact_conversation (take first, reject duplicates)
        {
            let mut compact_iter = resp.tool_calls.iter()
                .filter(|c| c.name == COMPACT_CONVERSATION_TOOL);
            if let Some(compact_call) = compact_iter.next() {
                dispatch_log(
                    config.hooks,
                    || serde_json::json!({
                        "level": "info",
                        "event": "tool_start",
                        "tool": "compact_conversation",
                    }),
                    "  ↳ compact_conversation(…)",
                )
                .await;
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

        // ── after_turn hooks ──
        if let Some(hooks) = config.hooks {
            let ctx = serde_json::json!({
                "turn": turn_num + 1,
                "tool_count": turns.last().map_or(0, |t: &TurnRecord| t.tool_calls.len()),
            });
            let _ = hooks.dispatch(HookEvent::AfterTurn, &ctx).await;
        }
    }

    let outcome = outcome.unwrap_or(LoopOutcome::MaxTurnsExhausted);
    (turns, outcome, messages)
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
            max_turns: 32,
            hooks: None,
        };

        let messages = vec![ChatMessage::user_text("Hi")];
        let (turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert_eq!(turns.len(), 1);
        assert!(turns[0].tool_calls.is_empty());
        assert!(matches!(&outcome, LoopOutcome::Completed(Some(t)) if t == "Hello, world!"));
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
            max_turns: 32,
            hooks: None,
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].tool_calls.len(), 1);
        assert_eq!(turns[0].tool_calls[0].name, "test_tool");
        assert!(turns[1].tool_calls.is_empty());
        assert!(matches!(&outcome, LoopOutcome::Completed(Some(t)) if t == "Done!"));

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
            max_turns: 32,
            hooks: None,
        };

        let messages = vec![ChatMessage::user_text("Hi")];
        let (_turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert!(matches!(&outcome, LoopOutcome::LlmError(e) if e.contains("API overloaded")),
            "got: {outcome:?}");
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
            max_turns: 32,
            hooks: None,
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        // Tool error should be converted to ToolResult with is_error:true
        assert_eq!(turns[0].tool_calls.len(), 1);
        assert!(turns[0].tool_calls[0].is_error);
        // Loop should continue and get the final text
        assert!(matches!(&outcome, LoopOutcome::Completed(Some(t)) if t == "Recovered"));
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
            max_turns: 32,
            hooks: None,
        };

        let messages = vec![ChatMessage::user_text("Hi")];
        let (turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert!(turns.is_empty());
        assert!(matches!(outcome, LoopOutcome::Cancelled));
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
            max_turns: 32,
            hooks: None,
        };

        let messages = vec![ChatMessage::user_text("Hi")];
        let (turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert!(turns.is_empty());
        assert!(matches!(outcome, LoopOutcome::TimedOut));
    }

    #[tokio::test]
    async fn loop_stops_at_max_turns() {
        let max_turns = 8;
        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "test_tool".into(),
            input: serde_json::json!({}),
        };
        let llm = MockLlmProvider::always_tool_call(tool_call, max_turns + 5);
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
            max_turns,
            hooks: None,
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert_eq!(turns.len(), max_turns);
        assert!(matches!(outcome, LoopOutcome::MaxTurnsExhausted));
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
            max_turns: 32,
            hooks: None,
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
