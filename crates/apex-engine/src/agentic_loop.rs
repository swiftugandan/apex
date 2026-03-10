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

use crate::constants::MAX_TURNS;

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

        // Execute all tool calls concurrently
        let tool_futures: Vec<_> = resp.tool_calls.iter().map(|call| {
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
