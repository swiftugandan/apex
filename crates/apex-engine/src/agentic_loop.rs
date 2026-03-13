use futures::future::join_all;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use apex_core::config::CompactionSection;
use apex_core::context::TokenEstimator;
use apex_core::domain::{
    CacheHint, ChatMessage, CompletionRequest, ContentBlock, HookEvent, HookOutcome, LogEntry,
    MessageRole, SystemBlock, ToolCallRecord, TurnRecord,
};
use apex_core::ports::{
    ConversationCompactor, HookRegistry, LlmProvider, OrientationProvider, ToolRegistry,
    WorkingMemory,
};

use apex_core::summarize_json;

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
    /// Aggregate tool call budget exhausted.
    ToolCallBudgetExhausted,
}

/// Configuration bundle for the agentic loop.
pub struct LoopConfig<'a> {
    pub persona: &'a str,
    pub llm: &'a dyn LlmProvider,
    pub compactor: &'a dyn ConversationCompactor,
    pub tools: &'a dyn ToolRegistry,
    pub estimator: &'a Arc<Mutex<TokenEstimator>>,
    pub max_tool_result_bytes: usize,
    pub max_output_tokens: u32,
    /// Reserved token budget for model reasoning/thinking; subtracted from usable context.
    pub reserved_reasoning_tokens: u32,
    pub scratchpad: Option<&'a Arc<Mutex<apex_core::domain::Scratchpad>>>,
    pub memory: Option<&'a dyn WorkingMemory>,
    /// Optional cancellation token — checked before each turn.
    pub cancel: Option<&'a CancellationToken>,
    /// Optional wall-clock timeout for the entire loop.
    pub timeout: Option<Duration>,
    /// Maximum number of LLM turns in this loop.
    pub max_turns: usize,
    /// Optional lifecycle hook registry.
    pub hooks: Option<&'a dyn HookRegistry>,
    /// Maximum tool input size in bytes before rewriting in history.
    pub max_tool_input_bytes: usize,
    /// Scratch directory for spilling original tool inputs.
    pub scratch_dir: Option<PathBuf>,
    /// Compaction settings (preserve_turns, max_summary_tokens, spill_history).
    pub compaction: CompactionSection,
    /// Maximum tool calls allowed per single LLM turn.
    pub max_tool_calls_per_turn: usize,
    /// Maximum total tool calls across all turns.
    pub max_total_tool_calls: usize,
    /// Enable prompt caching hints for static system prompt and tool blocks.
    pub prompt_caching: bool,
    /// Optional per-turn orientation provider (injected from composition root).
    pub orientation: Option<&'a dyn OrientationProvider>,
}

/// Serialize `messages` as pretty-printed JSON directly to a file under `dir`,
/// avoiding intermediate string allocation via `BufWriter`.
fn spill_to_disk(dir: &Path, messages: &[ChatMessage]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = dir.join(format!("compaction-{ts}.json"));
    let file = std::fs::File::create(&path).map_err(|e| format!("create: {e}"))?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, messages).map_err(|e| format!("serialize: {e}"))?;
    Ok(path)
}

/// Estimate prompt tokens for the current message history.
async fn estimate_prompt_tokens(
    messages: &[ChatMessage],
    estimator: &Arc<Mutex<TokenEstimator>>,
) -> u32 {
    let prompt_text: String = messages
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    let est = estimator.lock().await;
    est.estimate(&prompt_text)
}

/// Automatically compact conversation history when estimated prompt tokens
/// reach ≥ 80% of usable input capacity (context window minus reserve and max output).
/// Returns `(estimated_tokens, Option<compacted_count>)` — the second element is
/// `Some(n)` when compaction fired and `n` messages were summarized.
async fn maybe_compact(
    messages: &mut Vec<ChatMessage>,
    config: &LoopConfig<'_>,
) -> (u32, Option<usize>) {
    let context_window = config.llm.context_window() as u32;
    let usable_input = context_window
        .saturating_sub(config.reserved_reasoning_tokens)
        .saturating_sub(config.max_output_tokens);
    let threshold = ((usable_input as f64) * 0.8).ceil() as u32;

    let estimated_tokens = estimate_prompt_tokens(messages, config.estimator).await;

    if threshold == 0 || estimated_tokens < threshold {
        return (estimated_tokens, None);
    }

    // Spill full conversation history to disk before compaction (Principle 5 & 6).
    // Run in spawn_blocking so sync filesystem I/O does not block the async executor.
    if config.compaction.spill_history {
        if let Some(ref scratch_dir) = config.scratch_dir {
            let dir = scratch_dir.clone();
            let messages_clone = messages.clone();
            let spill_result =
                tokio::task::spawn_blocking(move || spill_to_disk(&dir, &messages_clone)).await;
            match spill_result {
                Ok(Ok(path)) => {
                    let msg_count = messages.len();
                    let p = path.display();
                    dispatch_log(
                        config.hooks,
                        || {
                            serde_json::json!({
                                "level": "info",
                                "event": "compaction_spill",
                                "messages": msg_count,
                                "path": p.to_string(),
                            })
                        },
                        &format!("  spilled {msg_count} messages to {p}"),
                    )
                    .await;
                }
                Ok(Err(e)) => eprintln!("  compaction spill failed ({e})"),
                Err(e) => eprintln!("  compaction spill spawn_blocking failed: {e}"),
            }
        } else {
            eprintln!("  spill_history enabled but no scratch_dir configured, skipping spill");
        }
    }

    match config
        .compactor
        .compact(
            messages,
            config.compaction.preserve_turns,
            config.compaction.max_summary_tokens,
        )
        .await
    {
        Ok((compacted, count)) => {
            dispatch_log(
                config.hooks,
                || serde_json::json!({
                    "level": "info",
                    "event": "conversation_compacted",
                    "messages_compacted": count,
                    "estimated_tokens": estimated_tokens,
                    "threshold": threshold,
                }),
                &format!("  auto-compacted: {count} messages summarized ({estimated_tokens} tokens ≥ {threshold} threshold)"),
            )
            .await;
            *messages = compacted;
            // Re-estimate after compaction since messages changed
            let new_est = estimate_prompt_tokens(messages, config.estimator).await;
            (new_est, Some(count))
        }
        Err(reason) => {
            dispatch_log(
                config.hooks,
                || {
                    serde_json::json!({
                        "level": "warn",
                        "event": "conversation_compaction_failed",
                        "reason": &reason,
                    })
                },
                &format!("  auto-compaction skipped: {reason}"),
            )
            .await;
            (estimated_tokens, None)
        }
    }
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
    let mut total_tool_calls: usize = 0;
    let deadline = config.timeout.map(|d| Instant::now() + d);
    let cache_hint = if config.prompt_caching {
        CacheHint::Static
    } else {
        CacheHint::Dynamic
    };
    let mut compaction_info: Option<(usize, usize)> = None; // (messages_compacted, at_turn)
    let system_blocks = vec![SystemBlock {
        text: config.persona.to_string(),
        cache_hint,
    }];

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

        // ── auto-compaction ──
        let (estimated_prompt_tokens, compacted_count) = maybe_compact(&mut messages, config).await;
        if let Some(count) = compacted_count {
            compaction_info = Some((count, turn_num + 1));
        }

        // ── orientation (injected as user message to preserve system prompt cache) ──
        let context_window = config.llm.context_window();
        let orientation_injected = if let Some(provider) = config.orientation {
            if let Some(text) = provider
                .build(
                    turn_num + 1,
                    config.max_turns,
                    estimated_prompt_tokens,
                    context_window,
                    compaction_info,
                )
                .await
            {
                // Temporarily append to the last user message so it rides
                // alongside existing content without breaking alternation.
                if let Some(last) = messages.last_mut() {
                    if last.role == MessageRole::User {
                        last.content.push(ContentBlock::Text { text });
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        // Per-turn schema extraction: picks up newly loaded deferred tools
        let schemas = config.tools.schemas();

        // Cap max_tokens so prompt + reserve + output never exceed context window
        let effective_max_tokens = config
            .max_output_tokens
            .min(
                (context_window as u32)
                    .saturating_sub(estimated_prompt_tokens)
                    .saturating_sub(config.reserved_reasoning_tokens),
            )
            .max(1);

        let req = CompletionRequest {
            system_blocks: &system_blocks,
            messages: &messages,
            max_tokens: effective_max_tokens,
            temperature: Some(0.2),
            cache_tools: config.prompt_caching,
            reserved_reasoning_tokens: config.reserved_reasoning_tokens,
        };

        let resp = match config.llm.complete_with_tools(req, &schemas).await {
            Ok(r) => r,
            Err(err) => {
                // Clean up injected orientation before breaking
                if orientation_injected {
                    if let Some(last) = messages.last_mut() {
                        last.content.pop();
                    }
                }
                outcome = Some(LoopOutcome::LlmError(format!("{err}")));
                break;
            }
        };

        {
            let has_cache = resp.usage.cache_creation_input_tokens > 0
                || resp.usage.cache_read_input_tokens > 0;
            let msg = if has_cache {
                format!(
                    "  turn {}: {} tool call(s), {} input / {} output tokens (cache: {} created, {} read)",
                    turn_num + 1,
                    resp.tool_calls.len(),
                    resp.usage.input_tokens,
                    resp.usage.output_tokens,
                    resp.usage.cache_creation_input_tokens,
                    resp.usage.cache_read_input_tokens,
                )
            } else {
                format!(
                    "  turn {}: {} tool call(s), {} input / {} output tokens",
                    turn_num + 1,
                    resp.tool_calls.len(),
                    resp.usage.input_tokens,
                    resp.usage.output_tokens,
                )
            };
            let tool_count = resp.tool_calls.len();
            let input_toks = resp.usage.input_tokens;
            let output_toks = resp.usage.output_tokens;
            let cache_create = resp.usage.cache_creation_input_tokens;
            let cache_read = resp.usage.cache_read_input_tokens;
            let output_details = resp.usage.output_tokens_details;
            dispatch_log(
                config.hooks,
                || {
                    let mut payload = serde_json::json!({
                        "level": "info",
                        "event": "turn_summary",
                        "turn": turn_num + 1,
                        "tool_calls": tool_count,
                        "input_tokens": input_toks,
                        "output_tokens": output_toks,
                        "cache_creation_input_tokens": cache_create,
                        "cache_read_input_tokens": cache_read,
                    });
                    if let Some(d) = output_details {
                        if d.reasoning_tokens.is_some() {
                            payload["output_tokens_details"] = serde_json::json!({
                                "reasoning_tokens": d.reasoning_tokens,
                            });
                        }
                    }
                    payload
                },
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
            est.calibrate_output(&resp.usage);
        }

        // Warn when output approaches or exceeds reserved reasoning budget
        if config.reserved_reasoning_tokens > 0
            && resp.usage.output_tokens as f64 >= 0.9 * config.reserved_reasoning_tokens as f64
        {
            dispatch_log(
                config.hooks,
                || {
                    serde_json::json!({
                        "level": "warn",
                        "event": "token_reserve_warning",
                        "turn": turn_num + 1,
                        "output_tokens": resp.usage.output_tokens,
                        "reserved_reasoning_tokens": config.reserved_reasoning_tokens,
                    })
                },
                &format!(
                    "  token reserve near exhausted: {} output tokens (reserve {})",
                    resp.usage.output_tokens, config.reserved_reasoning_tokens,
                ),
            )
            .await;
        }

        // Remove injected orientation from last user message before mutating history
        if orientation_injected {
            if let Some(last) = messages.last_mut() {
                last.content.pop();
            }
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

        // Check before_tool_call hooks (must be sequential since hooks may block)
        let mut blocked_calls: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(hooks) = config.hooks {
            for call in &resp.tool_calls {
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
                            || {
                                serde_json::json!({
                                    "level": "warn",
                                    "event": "tool_blocked",
                                    "tool": &call.name,
                                    "id": &call.id,
                                    "reason": &msg,
                                })
                            },
                            &format!("  ↳ {}(…) BLOCKED: {}", call.name, msg),
                        )
                        .await;
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

        // Enforce per-turn and aggregate tool call limits
        let non_blocked: Vec<_> = resp
            .tool_calls
            .iter()
            .filter(|c| !blocked_calls.contains(&c.id))
            .collect();

        let remaining_budget = config.max_total_tool_calls.saturating_sub(total_tool_calls);
        let effective_limit = config.max_tool_calls_per_turn.min(remaining_budget);
        let split = non_blocked.len().min(effective_limit);
        let (allowed_calls, excess_calls) = non_blocked.split_at(split);

        // Generate error results for excess calls
        for call in excess_calls {
            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: "Tool call budget exceeded: too many tool calls in this turn or total budget exhausted".to_string(),
                is_error: true,
            });
            call_records.push(ToolCallRecord {
                name: call.name.clone(),
                input_summary: summarize_json(&call.input, 80),
                output_summary: "BUDGET_EXCEEDED".to_string(),
                is_error: true,
                duration_ms: 0,
            });
        }

        let tool_futures: Vec<_> = allowed_calls
            .iter()
            .map(|call| async move {
                dispatch_log(
                    config.hooks,
                    || {
                        serde_json::json!({
                            "level": "info",
                            "event": "tool_start",
                            "tool": &call.name,
                            "id": &call.id,
                        })
                    },
                    &format!("  ↳ {}(…)", call.name),
                )
                .await;
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
                (*call, result, start.elapsed())
            })
            .collect();

        let results = join_all(tool_futures).await;
        total_tool_calls += results.len();

        for (call, result, elapsed) in results {
            let duration_ms = elapsed.as_millis() as u64;

            call_records.push(ToolCallRecord {
                name: call.name.clone(),
                input_summary: summarize_json(&call.input, 80),
                output_summary: summarize_json(&result.output, 120),
                is_error: result.is_error,
                duration_ms,
            });

            // Post-execution: rewrite bulky tool inputs in history (Principle 5)
            if !result.is_error {
                if let Some(mut rewritten) =
                    config
                        .tools
                        .rewrite_input(call, &result, config.max_tool_input_bytes)
                {
                    // Principle 6: spill original input to scratch for debuggability
                    if let Some(scratch) = &config.scratch_dir {
                        let spill_path = scratch.join(format!("tool-input-{}.json", call.id));
                        let _ = std::fs::create_dir_all(scratch);
                        let _ = std::fs::write(
                            &spill_path,
                            serde_json::to_string_pretty(&call.input).unwrap_or_default(),
                        );
                        rewritten["_spilled"] =
                            serde_json::json!(spill_path.to_string_lossy().into_owned());
                    }
                    // Mutate the ToolUse block in the stored assistant message
                    if let Some(last_assistant) = messages
                        .iter_mut()
                        .rev()
                        .find(|m| m.role == MessageRole::Assistant)
                    {
                        for block in &mut last_assistant.content {
                            if let ContentBlock::ToolUse { id, input, .. } = block {
                                if *id == call.id {
                                    *input = rewritten;
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            let raw_content =
                serde_json::to_string(&result.output).unwrap_or_else(|_| "{}".to_string());

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
                let truncated =
                    apex_core::truncate_str(&final_content, config.max_tool_result_bytes);
                final_content = format!(
                    "{truncated}\n\n[truncated: {orig} bytes → {kept} bytes]",
                    orig = raw_content.len(),
                    kept = truncated.len()
                );
            }

            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: result.tool_use_id,
                content: final_content,
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

        // ── after_turn hooks ──
        if let Some(hooks) = config.hooks {
            let ctx = serde_json::json!({
                "turn": turn_num + 1,
                "tool_count": turns.last().map_or(0, |t: &TurnRecord| t.tool_calls.len()),
            });
            let _ = hooks.dispatch(HookEvent::AfterTurn, &ctx).await;
        }

        // ── aggregate tool call budget check ──
        if total_tool_calls >= config.max_total_tool_calls {
            outcome = Some(LoopOutcome::ToolCallBudgetExhausted);
            break;
        }
    }

    let outcome = outcome.unwrap_or(LoopOutcome::MaxTurnsExhausted);
    (turns, outcome, messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_mocks::*;
    use apex_core::domain::ToolCall;

    fn default_estimator() -> Arc<Mutex<TokenEstimator>> {
        Arc::new(Mutex::new(TokenEstimator::default()))
    }

    fn test_compaction() -> CompactionSection {
        CompactionSection {
            preserve_turns: 3,
            max_summary_tokens: 1024,
            spill_history: false,
        }
    }

    fn default_compactor() -> MockConversationCompactor {
        MockConversationCompactor::new("Summary of conversation so far")
    }

    /// Build a `LoopConfig` with sensible test defaults. Tests override
    /// specific fields via struct update syntax (`.. base`).
    fn test_loop_config<'a>(
        llm: &'a dyn LlmProvider,
        compactor: &'a dyn apex_core::ports::ConversationCompactor,
        tools: &'a dyn ToolRegistry,
        estimator: &'a Arc<Mutex<TokenEstimator>>,
    ) -> LoopConfig<'a> {
        LoopConfig {
            persona: "You are helpful.",
            llm,
            compactor,
            tools,
            estimator,
            max_tool_result_bytes: 10_000,
            max_output_tokens: 4096,
            reserved_reasoning_tokens: 4096,
            scratchpad: None,
            memory: None,
            cancel: None,
            timeout: None,
            max_turns: 32,
            hooks: None,
            max_tool_input_bytes: 40_000,
            scratch_dir: None,
            compaction: test_compaction(),
            max_tool_calls_per_turn: 64,
            max_total_tool_calls: 512,
            prompt_caching: false,
            orientation: None,
        }
    }

    #[tokio::test]
    async fn loop_returns_on_end_turn() {
        let llm = MockLlmProvider::text_only("Hello, world!");
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();
        let compactor = default_compactor();
        let config = test_loop_config(&llm, &compactor, &tools, &estimator);

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
        let compactor = default_compactor();
        let config = test_loop_config(&llm, &compactor, &tools, &estimator);

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
        let compactor = default_compactor();
        let config = test_loop_config(&llm, &compactor, &tools, &estimator);

        let messages = vec![ChatMessage::user_text("Hi")];
        let (_turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert!(
            matches!(&outcome, LoopOutcome::LlmError(e) if e.contains("API overloaded")),
            "got: {outcome:?}"
        );
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
        let compactor = default_compactor();
        let config = test_loop_config(&llm, &compactor, &tools, &estimator);

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
        let compactor = default_compactor();
        let cancel = CancellationToken::new();
        cancel.cancel(); // Pre-cancel

        let config = LoopConfig {
            cancel: Some(&cancel),
            ..test_loop_config(&llm, &compactor, &tools, &estimator)
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
        let compactor = default_compactor();

        let config = LoopConfig {
            timeout: Some(Duration::from_secs(0)),
            ..test_loop_config(&llm, &compactor, &tools, &estimator)
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
        let compactor = default_compactor();

        let config = LoopConfig {
            max_turns,
            ..test_loop_config(&llm, &compactor, &tools, &estimator)
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert_eq!(turns.len(), max_turns);
        assert!(matches!(outcome, LoopOutcome::MaxTurnsExhausted));
    }

    // ── maybe_compact tests ─────────────────────────────

    /// Build a message set large enough to exceed a given token threshold.
    fn make_long_conversation(pairs: usize) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user_text("Original task description")];
        for i in 0..pairs {
            messages.push(ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("Assistant response number {i} with some extra context"),
                }],
            });
            messages.push(ChatMessage::user_text(format!(
                "User follow-up number {i} with additional detail"
            )));
        }
        messages
    }

    /// Helper to build a `LoopConfig` suitable for `maybe_compact` tests.
    /// Uses small reserve and max_output so that with a 200-token context window
    /// the usable-input threshold is positive and compaction can trigger.
    fn compact_test_config<'a>(
        llm: &'a dyn LlmProvider,
        compactor: &'a dyn apex_core::ports::ConversationCompactor,
        tools: &'a dyn ToolRegistry,
        estimator: &'a Arc<Mutex<TokenEstimator>>,
        compaction: CompactionSection,
        scratch_dir: Option<PathBuf>,
    ) -> LoopConfig<'a> {
        LoopConfig {
            persona: "",
            compaction,
            scratch_dir,
            reserved_reasoning_tokens: 0,
            max_output_tokens: 10,
            ..test_loop_config(llm, compactor, tools, estimator)
        }
    }

    #[tokio::test]
    async fn maybe_compact_triggers_above_threshold() {
        let messages = make_long_conversation(20);
        let summary_response = Ok(apex_core::domain::ToolCompletionResponse {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Summary of conversation so far".to_string(),
                }],
            },
            tool_calls: vec![],
            usage: apex_core::domain::TokenUsage {
                input_tokens: 100,
                output_tokens: 30,
                ..Default::default()
            },
            stop_reason: apex_core::domain::StopReason::EndTurn,
        });
        let llm = MockLlmProvider::new(vec![summary_response]).with_context_window(200);
        let tools = MockToolRegistry::echo("t");
        let estimator = default_estimator();
        let compactor = default_compactor();
        let cfg = compact_test_config(
            &llm,
            &compactor,
            &tools,
            &estimator,
            test_compaction(),
            None,
        );

        let mut msgs = messages.clone();
        let original_len = msgs.len();

        let _estimated = maybe_compact(&mut msgs, &cfg).await;

        assert!(
            msgs.len() < original_len,
            "should have triggered compaction"
        );
        assert_eq!(msgs[0].text(), "Original task description");
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert!(msgs[1].text().contains("compacted"));
        for i in 1..msgs.len() {
            assert_ne!(
                msgs[i].role,
                msgs[i - 1].role,
                "alternation violated at index {i}"
            );
        }
    }

    #[tokio::test]
    async fn maybe_compact_skips_below_threshold() {
        let llm = MockLlmProvider::text_only("should not be called").with_context_window(1_000_000);
        let tools = MockToolRegistry::echo("t");
        let estimator = default_estimator();
        let compactor = default_compactor();
        let cfg = compact_test_config(
            &llm,
            &compactor,
            &tools,
            &estimator,
            test_compaction(),
            None,
        );

        let mut msgs = vec![
            ChatMessage::user_text("Hi"),
            ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Hello".into(),
                }],
            },
            ChatMessage::user_text("How are you?"),
        ];
        let original_len = msgs.len();

        let _estimated = maybe_compact(&mut msgs, &cfg).await;

        assert_eq!(
            msgs.len(),
            original_len,
            "should NOT have triggered compaction"
        );
    }

    #[tokio::test]
    async fn maybe_compact_preserves_recent_turns() {
        let messages = make_long_conversation(15);
        let summary_response = Ok(apex_core::domain::ToolCompletionResponse {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Summarized older messages".to_string(),
                }],
            },
            tool_calls: vec![],
            usage: apex_core::domain::TokenUsage {
                input_tokens: 80,
                output_tokens: 20,
                ..Default::default()
            },
            stop_reason: apex_core::domain::StopReason::EndTurn,
        });
        let llm = MockLlmProvider::new(vec![summary_response]).with_context_window(200);
        let tools = MockToolRegistry::echo("t");
        let estimator = default_estimator();
        let compactor = default_compactor();
        let compaction = CompactionSection {
            preserve_turns: 2,
            ..test_compaction()
        };
        let cfg = compact_test_config(&llm, &compactor, &tools, &estimator, compaction, None);

        let mut msgs = messages.clone();
        let _estimated = maybe_compact(&mut msgs, &cfg).await;

        assert!(
            msgs.len() < messages.len(),
            "should have triggered compaction"
        );
        let compacted_tail = &msgs[2..];
        let original_tail = &messages[messages.len() - compacted_tail.len()..];
        assert!(
            !compacted_tail.is_empty(),
            "should have preserved at least some recent turns"
        );
        for (i, (orig, comp)) in original_tail.iter().zip(compacted_tail.iter()).enumerate() {
            assert_eq!(orig.role, comp.role, "tail message {i} role mismatch");
            assert_eq!(orig.text(), comp.text(), "tail message {i} text mismatch");
        }
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
        let compactor = default_compactor();

        let config = LoopConfig {
            max_tool_result_bytes: 1_000,
            ..test_loop_config(&llm, &compactor, &tools, &estimator)
        };

        let messages = vec![ChatMessage::user_text("Get data")];
        let (_turns, _final_text, msgs) = run_agentic_loop(messages, &config).await;

        let tool_result_content = msgs
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| {
                if let ContentBlock::ToolResult { content, .. } = b {
                    Some(content.clone())
                } else {
                    None
                }
            })
            .expect("should have a tool result message");

        assert!(
            tool_result_content.contains("[truncated:"),
            "output should contain truncation marker, got len={}",
            tool_result_content.len()
        );
    }

    /// Build a conversation with tool-use and tool-result blocks for richer roundtrip testing.
    fn make_conversation_with_tool_use(pairs: usize) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user_text("Original task description")];
        for i in 0..pairs {
            if i % 3 == 1 {
                messages.push(ChatMessage {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: format!("call-{i}"),
                        name: "test_tool".into(),
                        input: serde_json::json!({ "query": format!("lookup {i}") }),
                    }],
                });
                messages.push(ChatMessage {
                    role: MessageRole::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: format!("call-{i}"),
                        content: format!("result for lookup {i}"),
                        is_error: false,
                    }],
                });
            } else {
                messages.push(ChatMessage {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: format!("Assistant response number {i} with some extra context"),
                    }],
                });
                messages.push(ChatMessage::user_text(format!(
                    "User follow-up number {i} with additional detail"
                )));
            }
        }
        messages
    }

    #[tokio::test]
    async fn maybe_compact_spills_before_compaction() {
        let messages = make_conversation_with_tool_use(20);
        let summary_response = Ok(apex_core::domain::ToolCompletionResponse {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Summary of conversation so far".to_string(),
                }],
            },
            tool_calls: vec![],
            usage: apex_core::domain::TokenUsage {
                input_tokens: 100,
                output_tokens: 30,
                ..Default::default()
            },
            stop_reason: apex_core::domain::StopReason::EndTurn,
        });
        let llm = MockLlmProvider::new(vec![summary_response]).with_context_window(200);
        let tools = MockToolRegistry::echo("t");
        let estimator = default_estimator();
        let compactor = default_compactor();

        let tmp = tempfile::tempdir().unwrap();
        let compaction = CompactionSection {
            spill_history: true,
            ..test_compaction()
        };
        let cfg = compact_test_config(
            &llm,
            &compactor,
            &tools,
            &estimator,
            compaction,
            Some(tmp.path().to_path_buf()),
        );

        let mut msgs = messages.clone();
        let original_len = msgs.len();

        let _estimated = maybe_compact(&mut msgs, &cfg).await;

        assert!(
            msgs.len() < original_len,
            "should have triggered compaction"
        );

        // Verify a compaction-*.json file was written
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("compaction-"))
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one spill file");

        // Verify it deserializes back to the original messages (including tool-use blocks)
        let content = std::fs::read_to_string(entries[0].path()).unwrap();
        let deserialized: Vec<ChatMessage> = serde_json::from_str(&content).unwrap();
        assert_eq!(deserialized.len(), original_len);
        for (i, (orig, spilled)) in messages.iter().zip(deserialized.iter()).enumerate() {
            assert_eq!(orig.role, spilled.role, "role mismatch at index {i}");
            assert_eq!(
                orig.content.len(),
                spilled.content.len(),
                "content block count mismatch at index {i}"
            );
            for (j, (ob, sb)) in orig.content.iter().zip(spilled.content.iter()).enumerate() {
                match (ob, sb) {
                    (ContentBlock::Text { text: a }, ContentBlock::Text { text: b }) => {
                        assert_eq!(a, b, "text mismatch at msg {i} block {j}");
                    }
                    (
                        ContentBlock::ToolUse {
                            id: a_id,
                            name: a_name,
                            input: a_input,
                        },
                        ContentBlock::ToolUse {
                            id: b_id,
                            name: b_name,
                            input: b_input,
                        },
                    ) => {
                        assert_eq!(a_id, b_id, "tool_use id mismatch at msg {i} block {j}");
                        assert_eq!(
                            a_name, b_name,
                            "tool_use name mismatch at msg {i} block {j}"
                        );
                        assert_eq!(
                            a_input, b_input,
                            "tool_use input mismatch at msg {i} block {j}"
                        );
                    }
                    (
                        ContentBlock::ToolResult {
                            tool_use_id: a_id,
                            content: a_c,
                            is_error: a_e,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: b_id,
                            content: b_c,
                            is_error: b_e,
                        },
                    ) => {
                        assert_eq!(a_id, b_id, "tool_result id mismatch at msg {i} block {j}");
                        assert_eq!(
                            a_c, b_c,
                            "tool_result content mismatch at msg {i} block {j}"
                        );
                        assert_eq!(
                            a_e, b_e,
                            "tool_result is_error mismatch at msg {i} block {j}"
                        );
                    }
                    _ => panic!("content block variant mismatch at msg {i} block {j}"),
                }
            }
        }
    }

    // ── tool call budget tests ─────────────────────────────

    #[tokio::test]
    async fn loop_caps_tool_calls_per_turn() {
        // LLM returns 5 tool calls in one turn, but limit is 2
        let calls: Vec<ToolCall> = (0..5)
            .map(|i| ToolCall {
                id: format!("call-{i}"),
                name: "test_tool".into(),
                input: serde_json::json!({}),
            })
            .collect();
        let llm = MockLlmProvider::multi_tool_then_text(calls, "Done!");
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();
        let compactor = default_compactor();

        let config = LoopConfig {
            max_tool_calls_per_turn: 2,
            ..test_loop_config(&llm, &compactor, &tools, &estimator)
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        // Only 2 tool calls should have been executed
        let executed = tools.calls.lock().await;
        assert_eq!(executed.len(), 2, "expected only 2 tool calls to execute");

        // Turn record should show 5 total (2 executed + 3 budget-exceeded)
        assert_eq!(turns[0].tool_calls.len(), 5);
        let budget_exceeded: Vec<_> = turns[0]
            .tool_calls
            .iter()
            .filter(|tc| tc.output_summary == "BUDGET_EXCEEDED")
            .collect();
        assert_eq!(
            budget_exceeded.len(),
            3,
            "expected 3 budget-exceeded records"
        );

        assert!(matches!(&outcome, LoopOutcome::Completed(Some(t)) if t == "Done!"));
    }

    #[tokio::test]
    async fn loop_terminates_on_total_budget() {
        // Each turn makes 2 tool calls, total budget is 3, so should exhaust after 2 turns
        let calls: Vec<ToolCall> = (0..2)
            .map(|i| ToolCall {
                id: format!("call-{i}"),
                name: "test_tool".into(),
                input: serde_json::json!({}),
            })
            .collect();
        let llm = MockLlmProvider::always_multi_tool_calls(calls, 10);
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();
        let compactor = default_compactor();

        let config = LoopConfig {
            max_total_tool_calls: 3,
            ..test_loop_config(&llm, &compactor, &tools, &estimator)
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (_turns, outcome, _msgs) = run_agentic_loop(messages, &config).await;

        assert!(
            matches!(outcome, LoopOutcome::ToolCallBudgetExhausted),
            "expected ToolCallBudgetExhausted, got: {outcome:?}"
        );

        // Should have executed at most 3 tool calls total
        let executed = tools.calls.lock().await;
        assert!(
            executed.len() <= 3,
            "expected at most 3 executed calls, got {}",
            executed.len()
        );
    }

    #[tokio::test]
    async fn per_turn_limit_considers_remaining_budget() {
        // per_turn=10, total=5, LLM returns 8 calls → only 5 should execute
        let calls: Vec<ToolCall> = (0..8)
            .map(|i| ToolCall {
                id: format!("call-{i}"),
                name: "test_tool".into(),
                input: serde_json::json!({}),
            })
            .collect();
        let llm = MockLlmProvider::multi_tool_then_text(calls, "Done!");
        let tools = MockToolRegistry::echo("test_tool");
        let estimator = default_estimator();
        let compactor = default_compactor();

        let config = LoopConfig {
            max_tool_calls_per_turn: 10,
            max_total_tool_calls: 5,
            ..test_loop_config(&llm, &compactor, &tools, &estimator)
        };

        let messages = vec![ChatMessage::user_text("Do something")];
        let (turns, _outcome, _msgs) = run_agentic_loop(messages, &config).await;

        let executed = tools.calls.lock().await;
        assert_eq!(
            executed.len(),
            5,
            "expected 5 tool calls (capped by remaining budget)"
        );

        // 3 excess should be budget-exceeded
        let budget_exceeded: Vec<_> = turns[0]
            .tool_calls
            .iter()
            .filter(|tc| tc.output_summary == "BUDGET_EXCEEDED")
            .collect();
        assert_eq!(budget_exceeded.len(), 3);
    }
}
