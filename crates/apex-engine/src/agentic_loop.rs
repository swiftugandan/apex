use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use apex_core::context::TokenEstimator;
use apex_core::domain::{
    ChatMessage, CompletionRequest, ContentBlock, LogEntry, MessageRole,
    ToolCallRecord, TurnRecord,
};
use apex_core::ports::{LlmProvider, MemoryStore, ToolRegistry, WorkingMemory};

use crate::util::summarize_json;

const MAX_TURNS: usize = 32;
const MAX_TOKENS: u32 = 8192;

/// Runs the multi-turn LLM + tool execution loop. Shared between
/// execute_task, execute_continuation, and the `delegate` tool (sub-agents).
pub async fn run_agentic_loop(
    initial_messages: Vec<ChatMessage>,
    persona: &str,
    llm: &dyn LlmProvider,
    tools: &dyn ToolRegistry,
    long_term: &dyn MemoryStore,
    estimator: &Arc<Mutex<TokenEstimator>>,
    max_tool_result_bytes: usize,
    scratchpad: Option<&Mutex<apex_core::domain::Scratchpad>>,
    memory: Option<&dyn WorkingMemory>,
) -> (Vec<TurnRecord>, Option<String>, Vec<ChatMessage>) {
    let mut messages = initial_messages;
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut final_text: Option<String> = None;

    for turn_num in 0..MAX_TURNS {
        let schemas = tools.schemas();

        let req = CompletionRequest {
            system_prompt: persona.to_string(),
            messages: messages.clone(),
            max_tokens: MAX_TOKENS,
            temperature: Some(0.2),
        };

        let resp = match llm.complete_with_tools(req, &schemas).await {
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

        // Calibrate token estimator
        {
            let prompt_text: String = messages
                .iter()
                .map(|m| m.text())
                .collect::<Vec<_>>()
                .join("\n");
            let mut est = estimator.lock().await;
            est.calibrate(&prompt_text, resp.usage.input_tokens);
            if est.calibration_data().sample_count % 5 == 0 {
                let cal = est.calibration_data().clone();
                drop(est);
                let _ = long_term.persist_calibration(&cal).await;
            }
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

        for call in &resp.tool_calls {
            eprintln!("  ↳ {}(…)", call.name);
            let start = Instant::now();

            let result = match tools.execute(call).await {
                Ok(r) => r,
                Err(err) => apex_core::domain::ToolResult {
                    tool_use_id: call.id.clone(),
                    name: call.name.clone(),
                    output: serde_json::json!({ "error": err.to_string() }),
                    is_error: true,
                    ..Default::default()
                },
            };

            let duration_ms = start.elapsed().as_millis() as u64;

            call_records.push(ToolCallRecord {
                name: call.name.clone(),
                input_summary: summarize_json(&call.input, 80),
                output_summary: summarize_json(&result.output, 120),
                is_error: result.is_error,
                duration_ms,
            });

            let raw_content = serde_json::to_string(&result.output)
                .unwrap_or_else(|_| "{}".to_string());
            let content = if raw_content.len() > max_tool_result_bytes {
                let truncated = apex_core::truncate_str(&raw_content, max_tool_result_bytes);
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
        if let (Some(pad_mutex), Some(mem)) = (scratchpad, memory) {
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
