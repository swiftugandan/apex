use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;

use apex_core::config::Invariants;
use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::domain::{
    AttemptOutcome, AttemptRecord, ChatMessage, ClaimedTask, CompletionRequest, ContentBlock,
    Fact, FactId, MessageRole, MessageType, Skill, SkillId, Strategy, StrategyId,
    ToolCall, ToolCallRecord, ToolDef, ToolResult, TurnRecord,
};
use apex_core::error::ToolError;
use apex_core::ports::{LlmProvider, MemoryStore, Queue, ToolRegistry, WorkingMemory};
use apex_infra::{AnthropicProvider, RfbmqAdapter};

use crate::tools::{
    BuiltinToolRegistry, ConfigToolRegistry, CustomToolRegistry, MemoryToolRegistry,
    QueueToolRegistry,
};
use crate::tools::spill::SpillManager;

const MAX_TURNS: usize = 32;
const MAX_TOKENS: u32 = 8192;

// ── WorkerContext ───────────────────────────────────────────────────

pub struct WorkerContext {
    pub adapter: Arc<RfbmqAdapter>,
    pub queue: Arc<dyn Queue>,
    pub llm: Arc<AnthropicProvider>,
    pub eval_llm: Arc<dyn LlmProvider>,
    pub memory: Arc<dyn WorkingMemory>,
    pub long_term: Arc<dyn MemoryStore>,
    pub persona: Arc<String>,
    pub evaluator_persona: Arc<String>,
    pub eval_config: Arc<apex_eval::EvalConfig>,
    pub max_depth: u32,
    pub max_retries: u32,
    pub scratch_dir: PathBuf,
    pub tools_dir: PathBuf,
    pub config_dir: PathBuf,
    pub invariants: Arc<Invariants>,
    pub estimator: Arc<Mutex<TokenEstimator>>,
}

// ── CompositeToolRegistry ───────────────────────────────────────────

struct CompositeToolRegistry {
    registries: Vec<Box<dyn ToolRegistry>>,
}

impl CompositeToolRegistry {
    fn new(registries: Vec<Box<dyn ToolRegistry>>) -> Self {
        Self { registries }
    }
}

#[async_trait]
impl ToolRegistry for CompositeToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        self.registries
            .iter()
            .flat_map(|r| r.definitions())
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> std::result::Result<ToolResult, ToolError> {
        for registry in &self.registries {
            let names: Vec<String> = registry
                .definitions()
                .iter()
                .map(|d| d.schema.name.clone())
                .collect();
            if names.iter().any(|n| n == &call.name) {
                return registry.execute(call).await;
            }
        }
        Err(ToolError::UnknownTool(call.name.clone()))
    }
}

// ── Worker loop ─────────────────────────────────────────────────────

pub async fn worker_loop(ctx: WorkerContext, worker_id: usize) -> Result<()> {
    let mut empty_cycles = 0u32;

    loop {
        let claimed = ctx
            .adapter
            .pop()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let claimed = match claimed {
            Some(c) => {
                empty_cycles = 0;
                c
            }
            None => {
                let depth = ctx
                    .adapter
                    .depth()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;

                if depth.pending + depth.processing == 0 {
                    return Ok(());
                }

                check_failed_deps(&ctx.adapter).await?;

                empty_cycles += 1;
                if empty_cycles > 300 {
                    eprintln!("[worker {worker_id}] giving up after {empty_cycles} empty cycles");
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        let type_label = match claimed.headers.message_type {
            MessageType::Goal => "goal",
            MessageType::Task => "task",
            MessageType::Subtask => "subtask",
            MessageType::Continuation => "continuation",
        };
        eprintln!(
            "[worker {worker_id}] ▶ Processing {type_label} {} (depth {}, retry {})",
            claimed.id, claimed.headers.depth, claimed.headers.retry_count
        );

        // Build per-task tool registry
        let queue_tools = QueueToolRegistry::new(
            Arc::clone(&ctx.queue),
            claimed.headers.correlation_id.clone(),
            claimed.headers.depth,
            ctx.max_depth,
            extract_title(&claimed.body),
            claimed.body.clone(),
            Some(Arc::clone(&ctx.long_term)),
        );

        let memory_tools = MemoryToolRegistry::new(
            Arc::clone(&ctx.memory),
            Arc::clone(&ctx.long_term),
        );
        let custom_spill = SpillManager::new(ctx.scratch_dir.clone());
        let custom_tools = CustomToolRegistry::new(
            ctx.tools_dir.clone(),
            custom_spill,
            Some(Arc::clone(&ctx.long_term)),
        );
        let config_tools = ConfigToolRegistry::new(
            ctx.config_dir.clone(),
            Arc::clone(&ctx.invariants),
        );
        let tools = CompositeToolRegistry::new(vec![
            Box::new(BuiltinToolRegistry::new(ctx.scratch_dir.clone())),
            Box::new(memory_tools),
            Box::new(queue_tools),
            Box::new(custom_tools),
            Box::new(config_tools),
        ]);

        let composer = {
            let est = ctx.estimator.lock().await;
            MessageComposer::new(TokenEstimator::new(est.calibration_data().clone()))
        };

        let result = match claimed.headers.message_type {
            MessageType::Goal | MessageType::Task | MessageType::Subtask => {
                execute_task(&ctx, &claimed, &tools).await
            }
            MessageType::Continuation => {
                execute_continuation(&ctx, &claimed, &tools).await
            }
        };

        match result {
            Ok(record) => {
                let title = extract_title(&claimed.body);
                let result_body = MessageComposer::compose_result(&title, &record);
                ctx.adapter
                    .update_body(&claimed, &result_body)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                ctx.adapter
                    .ack(&claimed)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                eprintln!(
                    "[worker {worker_id}] ✓ {type_label} {} completed",
                    claimed.id
                );
            }
            Err((record, err, scratchpad)) => {
                handle_failure(
                    &ctx.adapter, &claimed, &record, &err, &scratchpad,
                    worker_id, &composer, ctx.max_retries,
                )
                .await?;
            }
        }
    }
}

// ── Shared agentic loop ─────────────────────────────────────────────

/// Runs the multi-turn LLM + tool execution loop. Shared between
/// execute_task and execute_continuation.
async fn run_agentic_loop(
    initial_messages: Vec<ChatMessage>,
    persona: &str,
    llm: &dyn LlmProvider,
    tools: &dyn ToolRegistry,
    long_term: &dyn MemoryStore,
    estimator: &Arc<Mutex<TokenEstimator>>,
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
                Err(err) => ToolResult {
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

            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: result.tool_use_id,
                content: serde_json::to_string(&result.output)
                    .unwrap_or_else(|_| "{}".to_string()),
                is_error: result.is_error,
            });
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

/// Evaluate the result and build the final AttemptRecord.
async fn evaluate_and_finalize(
    claimed: &ClaimedTask,
    turns: Vec<TurnRecord>,
    final_text: Option<String>,
    scratchpad: &apex_core::domain::Scratchpad,
    started_at: String,
    eval_llm: &dyn LlmProvider,
    eval_config: &apex_eval::EvalConfig,
    evaluator_persona: &str,
    long_term: &dyn MemoryStore,
) -> std::result::Result<AttemptRecord, (AttemptRecord, String, apex_core::domain::Scratchpad)> {
    let result_text = final_text.as_deref().unwrap_or("");
    let evaluation = apex_eval::Evaluator::evaluate(
        &claimed.body,
        result_text,
        evaluator_persona,
        eval_llm,
        eval_config,
    )
    .await;

    if !evaluation.passed {
        let summary = evaluation.failure_summary();
        let reason = if evaluation
            .deterministic
            .as_ref()
            .map_or(false, |d| !d.all_passed())
        {
            "deterministic evaluation failed"
        } else {
            "adversarial evaluation failed"
        };
        eprintln!("  eval: {reason}");
        let record = AttemptRecord {
            attempt_number: claimed.headers.retry_count + 1,
            started_at,
            finished_at: now_iso(),
            turns,
            final_text,
            outcome: AttemptOutcome::Failed,
            failure_reason: Some(reason.into()),
            eval_summary: Some(summary),
        };
        return Err((record, reason.into(), scratchpad.clone()));
    }

    let eval_summary =
        if evaluation.deterministic.is_some() || evaluation.adversarial.is_some() {
            Some(evaluation.full_summary())
        } else {
            None
        };

    if let Some(ref det) = evaluation.deterministic {
        eprintln!("  eval: {}/{} checks passed", det.passed, det.total);
    }
    if evaluation.adversarial.is_some() {
        eprintln!("  eval: adversarial passed");
    }

    let record = AttemptRecord {
        attempt_number: claimed.headers.retry_count + 1,
        started_at,
        finished_at: now_iso(),
        turns,
        final_text,
        outcome: AttemptOutcome::Success,
        failure_reason: None,
        eval_summary,
    };

    // Best-effort consolidation
    consolidate_learnings(
        long_term,
        &claimed.headers.correlation_id,
        &record,
        scratchpad,
    )
    .await;

    Ok(record)
}

// ── Task execution ──────────────────────────────────────────────────

async fn execute_task(
    ctx: &WorkerContext,
    claimed: &ClaimedTask,
    tools: &dyn ToolRegistry,
) -> std::result::Result<AttemptRecord, (AttemptRecord, String, apex_core::domain::Scratchpad)> {
    let started_at = now_iso();
    let job_id = &claimed.headers.correlation_id;

    let mut scratchpad = ctx
        .memory
        .load_or_create(job_id)
        .await
        .unwrap_or_else(|_| apex_core::domain::Scratchpad::new(job_id, ""));

    if scratchpad.goal.is_empty() {
        scratchpad.goal = extract_title(&claimed.body);
        let _ = ctx.memory.save(&scratchpad).await;
    }

    let initial_body = if !scratchpad.subtasks.is_empty() || !scratchpad.notes.is_empty() {
        format!(
            "{}\n\n---\n## Working Memory (from previous iterations)\n{}",
            claimed.body,
            scratchpad.to_markdown()
        )
    } else {
        claimed.body.clone()
    };

    let messages = vec![ChatMessage::user_text(&initial_body)];

    let (turns, final_text, _messages) = run_agentic_loop(
        messages,
        &ctx.persona,
        ctx.llm.as_ref(),
        tools,
        ctx.long_term.as_ref(),
        &ctx.estimator,
    )
    .await;

    // Check for LLM error
    if let Some(ref text) = final_text {
        if text.starts_with("LLM error:") {
            let record = AttemptRecord {
                attempt_number: claimed.headers.retry_count + 1,
                started_at,
                finished_at: now_iso(),
                turns,
                final_text: None,
                outcome: AttemptOutcome::Failed,
                failure_reason: Some(text.clone()),
                eval_summary: None,
            };
            return Err((record, text.clone(), scratchpad));
        }
    }

    // Reload scratchpad after execution
    if let Ok(updated_pad) = ctx.memory.load_or_create(job_id).await {
        scratchpad = updated_pad;
    }
    let _ = ctx.memory.save(&scratchpad).await;

    evaluate_and_finalize(
        claimed,
        turns,
        final_text,
        &scratchpad,
        started_at,
        ctx.eval_llm.as_ref(),
        &ctx.eval_config,
        &ctx.evaluator_persona,
        ctx.long_term.as_ref(),
    )
    .await
}

// ── Continuation execution ──────────────────────────────────────────

async fn execute_continuation(
    ctx: &WorkerContext,
    claimed: &ClaimedTask,
    tools: &dyn ToolRegistry,
) -> std::result::Result<AttemptRecord, (AttemptRecord, String, apex_core::domain::Scratchpad)> {
    let started_at = now_iso();
    let job_id = &claimed.headers.correlation_id;

    let scratchpad = ctx
        .memory
        .load_or_create(job_id)
        .await
        .unwrap_or_else(|_| apex_core::domain::Scratchpad::new(job_id, ""));

    // Read completed subtask results
    let done_ids = ctx
        .queue
        .list_done(job_id)
        .await
        .map_err(|e| {
            let record = AttemptRecord {
                attempt_number: claimed.headers.retry_count + 1,
                started_at: started_at.clone(),
                finished_at: now_iso(),
                turns: vec![],
                final_text: None,
                outcome: AttemptOutcome::Failed,
                failure_reason: Some(format!("Failed to list done messages: {e}")),
                eval_summary: None,
            };
            (record, e.to_string(), scratchpad.clone())
        })?;

    let mut subtask_results = Vec::new();
    for id in &done_ids {
        if let Ok(body) = ctx.queue.read_done_body(id).await {
            subtask_results.push((id.clone(), body));
        }
    }

    let initial_body = format!(
        "{}\n\n---\n## Pre-loaded Results ({} subtasks completed)\n{}",
        claimed.body,
        subtask_results.len(),
        subtask_results
            .iter()
            .map(|(id, body)| format!("### {id}\n{body}\n"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let messages = vec![ChatMessage::user_text(&initial_body)];

    let (turns, final_text, _messages) = run_agentic_loop(
        messages,
        &ctx.persona,
        ctx.llm.as_ref(),
        tools,
        ctx.long_term.as_ref(),
        &ctx.estimator,
    )
    .await;

    // Check for LLM error
    if let Some(ref text) = final_text {
        if text.starts_with("LLM error:") {
            let record = AttemptRecord {
                attempt_number: claimed.headers.retry_count + 1,
                started_at,
                finished_at: now_iso(),
                turns,
                final_text: None,
                outcome: AttemptOutcome::Failed,
                failure_reason: Some(text.clone()),
                eval_summary: None,
            };
            return Err((record, text.clone(), scratchpad));
        }
    }

    evaluate_and_finalize(
        claimed,
        turns,
        final_text,
        &scratchpad,
        started_at,
        ctx.eval_llm.as_ref(),
        &ctx.eval_config,
        &ctx.evaluator_persona,
        ctx.long_term.as_ref(),
    )
    .await
}

// ── Failure handling ────────────────────────────────────────────────

async fn handle_failure(
    adapter: &RfbmqAdapter,
    claimed: &ClaimedTask,
    record: &AttemptRecord,
    err: &str,
    scratchpad: &apex_core::domain::Scratchpad,
    worker_id: usize,
    composer: &MessageComposer,
    max_retries: u32,
) -> Result<()> {
    eprintln!("[worker {worker_id}] ✗ {} failed: {err}", claimed.id);

    let updated_body =
        composer.append_attempt_with_memory(&claimed.body, record, scratchpad);

    adapter
        .update_body(claimed, &updated_body)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    adapter
        .nack(claimed)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if claimed.headers.retry_count + 1 >= max_retries {
        eprintln!("[worker {worker_id}]   ↳ Max retries reached, message moved to failed/");
    } else {
        eprintln!(
            "[worker {worker_id}]   ↳ Requeued for retry (attempt {} of {})",
            claimed.headers.retry_count + 2,
            max_retries
        );
    }
    Ok(())
}

/// Check if any pending messages have dependencies in failed/.
async fn check_failed_deps(adapter: &RfbmqAdapter) -> Result<()> {
    let _ = adapter;
    Ok(())
}

// ── Consolidation ───────────────────────────────────────────────────

async fn consolidate_learnings(
    store: &dyn MemoryStore,
    correlation_id: &str,
    record: &AttemptRecord,
    scratchpad: &apex_core::domain::Scratchpad,
) {
    // 1. Extract facts from "## New Facts Discovered" sections
    if let Some(ref text) = record.final_text {
        let mut in_facts_section = false;
        for line in text.lines() {
            if line.contains("New Facts Discovered") || line.contains("new facts discovered") {
                in_facts_section = true;
                continue;
            }
            if in_facts_section && line.starts_with("## ") {
                break;
            }
            if in_facts_section {
                if let Some(content) = line.strip_prefix("- ") {
                    let content = content.trim();
                    if !content.is_empty() {
                        let fact = Fact {
                            id: FactId(String::new()),
                            content: content.to_string(),
                            source_job: correlation_id.to_string(),
                            confidence: 0.8,
                            created_at: String::new(),
                            last_verified: String::new(),
                            tags: vec![],
                        };
                        if let Err(e) = store.store_fact(fact).await {
                            eprintln!("  consolidation: failed to store fact: {e}");
                        }
                    }
                }
            }
        }
    }

    // 2. Skills: update fitness for successful tasks
    let title = &scratchpad.goal;
    if !title.is_empty() {
        match store.find_skill(title).await {
            Ok(Some(skill)) => {
                if let Err(e) = store
                    .update_skill_fitness(&skill.id, record.outcome == AttemptOutcome::Success)
                    .await
                {
                    eprintln!("  consolidation: failed to update skill fitness: {e}");
                }
            }
            Ok(None) => {
                let tools_used: Vec<String> = record
                    .turns
                    .iter()
                    .flat_map(|t| t.tool_calls.iter())
                    .map(|tc| tc.name.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();

                if !tools_used.is_empty() && record.outcome == AttemptOutcome::Success {
                    let skill = Skill {
                        id: SkillId(String::new()),
                        task_pattern: title.to_string(),
                        approach: record
                            .final_text
                            .as_deref()
                            .unwrap_or("")
                            .lines()
                            .take(3)
                            .collect::<Vec<_>>()
                            .join(" "),
                        tools_used,
                        criteria_template: None,
                        success_count: 1,
                        failure_count: 0,
                        fitness: 0.5,
                        min_samples: 3,
                        last_used: String::new(),
                        notes: String::new(),
                    };
                    if let Err(e) = store.store_skill(skill).await {
                        eprintln!("  consolidation: failed to store skill: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("  consolidation: failed to find skill: {e}");
            }
        }
    }

    // 3. Strategies: for jobs with subtasks
    if !scratchpad.subtasks.is_empty() && !scratchpad.goal.is_empty() {
        let decomposition = scratchpad
            .subtasks
            .iter()
            .map(|st| format!("{}. {}", st.index, st.description))
            .collect::<Vec<_>>()
            .join("\n");

        match store.find_strategy(&scratchpad.goal).await {
            Ok(Some(strategy)) => {
                let success = scratchpad
                    .subtasks
                    .iter()
                    .all(|st| st.status == apex_core::domain::SubtaskStatus::Done);
                if let Err(e) = store.update_strategy_fitness(&strategy.id, success).await {
                    eprintln!("  consolidation: failed to update strategy fitness: {e}");
                }
            }
            Ok(None) => {
                let strategy = Strategy {
                    id: StrategyId(String::new()),
                    goal_pattern: scratchpad.goal.clone(),
                    decomposition,
                    avg_subtasks: scratchpad.subtasks.len() as f64,
                    avg_duration_secs: 0.0,
                    success_count: if record.outcome == AttemptOutcome::Success {
                        1
                    } else {
                        0
                    },
                    failure_count: if record.outcome == AttemptOutcome::Failed {
                        1
                    } else {
                        0
                    },
                    fitness: 0.5,
                    notes: String::new(),
                };
                if let Err(e) = store.store_strategy(strategy).await {
                    eprintln!("  consolidation: failed to store strategy: {e}");
                }
            }
            Err(e) => {
                eprintln!("  consolidation: failed to find strategy: {e}");
            }
        }
    }
}

// ── Utilities ───────────────────────────────────────────────────────

pub fn extract_title(body: &str) -> String {
    for line in body.lines() {
        if let Some(title) = line.strip_prefix("# Task: ") {
            return title.to_string();
        }
        if let Some(title) = line.strip_prefix("# Subtask: ") {
            return title.to_string();
        }
        if let Some(title) = line.strip_prefix("# Continuation: ") {
            return title.to_string();
        }
        if let Some(title) = line.strip_prefix("# ") {
            return title.to_string();
        }
    }
    "Untitled".to_string()
}

fn summarize_json(value: &serde_json::Value, max_len: usize) -> String {
    let s = value.to_string();
    if s.len() <= max_len {
        s
    } else {
        format!("{}…", &s[..max_len])
    }
}

fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}
