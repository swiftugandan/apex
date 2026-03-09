use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::Result;

use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::domain::{
    AttemptOutcome, AttemptRecord, ChatMessage, ClaimedTask, MessageType,
};
use apex_core::ports::{LlmProvider, MemoryStore, Queue, ToolRegistry, WorkingMemory};

use apex_tools::QueueToolRegistry;

use crate::agentic_loop::run_agentic_loop;
use crate::consolidation::consolidate_learnings;
use crate::util::{composer_from_estimator, extract_title, now_iso};

// ── WorkerContext ───────────────────────────────────────────────────

#[derive(Clone)]
pub struct WorkerContext {
    pub queue: Arc<dyn Queue>,
    pub tools: Arc<dyn ToolRegistry>,
    pub llm: Arc<dyn LlmProvider>,
    pub memory: Arc<dyn WorkingMemory>,
    pub long_term: Arc<dyn MemoryStore>,
    pub persona: Arc<String>,
    pub max_depth: u32,
    pub max_retries: u32,
    pub max_tool_result_bytes: usize,
    pub estimator: Arc<Mutex<TokenEstimator>>,
}

// ── Worker loop ─────────────────────────────────────────────────────

pub async fn worker_loop(ctx: WorkerContext, worker_id: usize) -> Result<()> {
    let mut empty_cycles = 0u32;

    loop {
        let claimed = ctx
            .queue
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
                    .queue
                    .depth()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;

                if depth.pending + depth.processing == 0 {
                    return Ok(());
                }

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

        // Build per-claim tool registry (static tools + queue tools)
        let composer = composer_from_estimator(&ctx.estimator).await;
        let queue_tools = QueueToolRegistry::new(
            Arc::clone(&ctx.queue),
            claimed.headers.correlation_id.clone(),
            claimed.headers.depth,
            ctx.max_depth,
            extract_title(&claimed.body),
            claimed.body.clone(),
            Some(Arc::clone(&ctx.long_term)),
            composer,
        );

        // Wrap static tools into ApexToolRegistry with queue tools
        // We need to downcast Arc<dyn ToolRegistry> to get the composite back.
        // Instead, we'll use the tools directly since ApexToolRegistry works with any ToolRegistry.
        let tools = PerClaimToolRegistry {
            static_tools: Arc::clone(&ctx.tools),
            queue_tools,
        };

        let result = execute_claim(&ctx, &claimed, &tools).await;

        match result {
            Ok(record) => {
                let title = extract_title(&claimed.body);
                let result_body = MessageComposer::compose_result(&title, &record);
                ctx.queue
                    .update_body(&claimed, &result_body)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                ctx.queue
                    .ack(&claimed)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                eprintln!(
                    "[worker {worker_id}] ✓ {type_label} {} completed",
                    claimed.id
                );
            }
            Err((record, err, scratchpad)) => {
                let composer = composer_from_estimator(&ctx.estimator).await;
                handle_failure(
                    ctx.queue.as_ref(), &claimed, &record, &err, &scratchpad,
                    worker_id, &composer, ctx.max_retries,
                )
                .await?;
            }
        }
    }
}

/// Per-claim tool registry that combines static tools with queue-specific tools.
struct PerClaimToolRegistry {
    static_tools: Arc<dyn ToolRegistry>,
    queue_tools: QueueToolRegistry,
}

#[async_trait::async_trait]
impl ToolRegistry for PerClaimToolRegistry {
    fn definitions(&self) -> Vec<apex_core::domain::ToolDef> {
        let mut defs = self.static_tools.definitions();
        defs.extend(self.queue_tools.definitions());
        defs
    }

    async fn execute(&self, call: &apex_core::domain::ToolCall) -> std::result::Result<apex_core::domain::ToolResult, apex_core::error::ToolError> {
        // Try static tools first by checking definitions
        let static_names: Vec<String> = self.static_tools.definitions().iter().map(|d| d.schema.name.clone()).collect();
        if static_names.iter().any(|n| n == &call.name) {
            return self.static_tools.execute(call).await;
        }
        self.queue_tools.execute(call).await
    }
}

// ── Unified claim execution ──────────────────────────────────────────

async fn execute_claim(
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

    let initial_body = {
        if claimed.headers.message_type != MessageType::Continuation && scratchpad.goal.is_empty() {
            scratchpad.goal = extract_title(&claimed.body);
            let _ = ctx.memory.save(&scratchpad).await;
        }
        if !scratchpad.subtasks.is_empty() || !scratchpad.notes.is_empty() {
            format!(
                "{}\n\n---\n## Working Memory (from previous iterations)\n{}",
                claimed.body,
                scratchpad.to_markdown()
            )
        } else {
            claimed.body.clone()
        }
    };

    let messages = vec![ChatMessage::user_text(&initial_body)];

    let scratchpad = Mutex::new(scratchpad);
    let (turns, final_text, _messages) = run_agentic_loop(
        messages,
        &ctx.persona,
        ctx.llm.as_ref(),
        tools,
        ctx.long_term.as_ref(),
        &ctx.estimator,
        ctx.max_tool_result_bytes,
        Some(&scratchpad),
        Some(ctx.memory.as_ref()),
    )
    .await;
    let mut scratchpad = scratchpad.into_inner();

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
            };
            return Err((record, text.clone(), scratchpad));
        }
    }

    if let Ok(updated_pad) = ctx.memory.load_or_create(job_id).await {
        // Preserve the log from our run, merge with any tool-updated fields
        let log = scratchpad.log.clone();
        scratchpad = updated_pad;
        scratchpad.log = log;
    }
    let _ = ctx.memory.save(&scratchpad).await;

    let record = AttemptRecord {
        attempt_number: claimed.headers.retry_count + 1,
        started_at,
        finished_at: now_iso(),
        turns,
        final_text,
        outcome: AttemptOutcome::Success,
        failure_reason: None,
    };

    // Best-effort consolidation
    consolidate_learnings(
        ctx.long_term.as_ref(),
        &claimed.headers.correlation_id,
        &record,
        &scratchpad,
    )
    .await;

    Ok(record)
}

// ── Failure handling ────────────────────────────────────────────────

/// Returns true if the error is non-retryable (e.g. auth, billing, invalid config).
fn is_non_retryable(err: &str) -> bool {
    let non_retryable_patterns = [
        "credit balance is too low",
        "invalid x-api-key",
        "invalid api key",
        "authentication_error",
        "permission_error",
        "not_found_error",
        "configuration error:",
    ];
    let lower = err.to_lowercase();
    non_retryable_patterns
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// Returns true if the error is a rate limit (429) that should be retried with backoff.
fn is_rate_limited(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("rate_limit") || lower.contains("429") || lower.contains("too many requests")
}

async fn handle_failure(
    queue: &dyn Queue,
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

    queue
        .update_body(claimed, &updated_body)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if is_non_retryable(err) {
        queue
            .reject(claimed)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        eprintln!("[worker {worker_id}]   ↳ Non-retryable error, moved to failed/");
        return Ok(());
    }

    // Rate limit errors: wait before requeueing so the next attempt doesn't
    // immediately hit the same limit.
    if is_rate_limited(err) {
        let backoff_secs = 30 * (claimed.headers.retry_count + 1) as u64;
        eprintln!(
            "[worker {worker_id}]   ↳ Rate limited, waiting {backoff_secs}s before retry"
        );
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
    }

    queue
        .nack(claimed)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if claimed.headers.retry_count + 1 >= max_retries {
        eprintln!("[worker {worker_id}]   ↳ Max retries reached, moved to failed/");
    } else {
        eprintln!(
            "[worker {worker_id}]   ↳ Requeued for retry (attempt {} of {})",
            claimed.headers.retry_count + 2,
            max_retries
        );
    }
    Ok(())
}
