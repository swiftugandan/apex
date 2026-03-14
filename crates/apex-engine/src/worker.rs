use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::Result;

use apex_core::config::{CompactionSection, ConsolidationSection};
use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::domain::{
    AttemptOutcome, AttemptRecord, ChatMessage, ClaimedTask, HookEvent, HookOutcome, LoopLimits,
    MessageType,
};
use apex_core::ports::{
    ConversationCompactor, HookRegistry, LlmProvider, MemoryStore, OrientationProvider, Queue,
    SkillExtractor, SkillStore, WorkingMemory,
};

use crate::agentic_loop::{run_agentic_loop, LoopConfig, LoopOutcome};
use crate::claim_tool_factory::{ClaimContext, ClaimToolFactory};
use crate::consolidation::consolidate_learnings;
use crate::jit_retrieval;
use crate::log::dispatch_log;
use crate::util::{composer_from_estimator, extract_title, now_unix_ts};

// ── WorkerContext ───────────────────────────────────────────────────

/// Scalar configuration limits grouped for cleaner construction.
#[derive(Clone)]
pub struct WorkerLimits {
    pub max_depth: u32,
    pub max_retries: u32,
    pub max_empty_cycles: u32,
    pub limits: LoopLimits,
}

#[derive(Clone)]
pub struct WorkerContext {
    pub queue: Arc<dyn Queue>,
    pub claim_tool_factory: Arc<dyn ClaimToolFactory>,
    pub llm: Arc<dyn LlmProvider>,
    pub compactor: Arc<dyn ConversationCompactor>,
    /// Used only during post-claim consolidation.
    pub skill_extractor: Option<Arc<dyn SkillExtractor>>,
    pub memory: Arc<dyn WorkingMemory>,
    pub long_term: Arc<dyn MemoryStore>,
    pub skills: Arc<dyn SkillStore>,
    pub persona: Arc<String>,
    pub limits: WorkerLimits,
    pub estimator: Arc<Mutex<TokenEstimator>>,
    pub compaction: CompactionSection,
    pub consolidation: ConsolidationSection,
    pub hooks: Option<Arc<dyn HookRegistry>>,
    /// Scratch directory for spilling original tool inputs before rewriting.
    pub scratch_dir: Option<std::path::PathBuf>,
    /// Factory for building per-claim orientation providers from the claim's scratchpad.
    pub orientation_factory: Option<Arc<dyn OrientationFactory>>,
}

/// Builds a per-claim `OrientationProvider` from the claim's scratchpad.
/// Implemented in the composition root (apex-bin).
pub trait OrientationFactory: Send + Sync {
    fn build(
        &self,
        scratchpad: Arc<Mutex<apex_core::domain::Scratchpad>>,
    ) -> Arc<dyn OrientationProvider>;
}

// ── Worker loop ─────────────────────────────────────────────────────

pub async fn worker_loop(ctx: WorkerContext, worker_id: usize) -> Result<()> {
    let mut empty_cycles = 0u32;

    loop {
        let claimed = ctx.queue.pop().await.map_err(|e| anyhow::anyhow!("{e}"))?;

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
                if empty_cycles > ctx.limits.max_empty_cycles {
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
        {
            let msg = format!(
                "[worker {worker_id}] ▶ Processing {type_label} {} (depth {}, retry {})",
                claimed.id, claimed.headers.depth, claimed.headers.retry_count
            );
            let id = &claimed.id;
            let depth = claimed.headers.depth;
            let retry = claimed.headers.retry_count;
            dispatch_log(
                ctx.hooks.as_deref(),
                || {
                    serde_json::json!({
                        "level": "info",
                        "event": "claim_start",
                        "worker_id": worker_id,
                        "message_type": type_label,
                        "message_id": id,
                        "depth": depth,
                        "retry_count": retry,
                    })
                },
                &msg,
            )
            .await;
        }

        let result = execute_claim(&ctx, &claimed).await;

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
                {
                    let msg = format!(
                        "[worker {worker_id}] ✓ {type_label} {} completed",
                        claimed.id
                    );
                    let id = &claimed.id;
                    dispatch_log(
                        ctx.hooks.as_deref(),
                        || {
                            serde_json::json!({
                                "level": "info",
                                "event": "claim_done",
                                "worker_id": worker_id,
                                "message_type": type_label,
                                "message_id": id,
                            })
                        },
                        &msg,
                    )
                    .await;
                }
            }
            Err((record, err, scratchpad)) => {
                let composer = composer_from_estimator(&ctx.estimator).await;
                handle_failure(
                    ctx.queue.as_ref(),
                    &claimed,
                    &record,
                    &err,
                    &scratchpad,
                    worker_id,
                    &composer,
                    ctx.limits.max_retries,
                    ctx.hooks.as_deref(),
                )
                .await?;
            }
        }
    }
}

// ── Unified claim execution ──────────────────────────────────────────

async fn execute_claim(
    ctx: &WorkerContext,
    claimed: &ClaimedTask,
) -> std::result::Result<AttemptRecord, (AttemptRecord, String, apex_core::domain::Scratchpad)> {
    let started_at = now_unix_ts();
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

    // ── Inject skill references ──
    let mut initial_body = initial_body;
    if !claimed.headers.skills.is_empty() {
        let mut skill_section = String::from("\n\n---\n## Available Skills\n");
        for s in &claimed.headers.skills {
            skill_section.push_str(&format!("- {} v{}\n", s.name, s.version));
        }
        skill_section.push_str("Use `use_skill(name=\"...\")` to load any of these.\n");
        initial_body.push_str(&skill_section);
    }

    // ── after_claim hooks ──
    if let Some(ref hooks) = ctx.hooks {
        let hook_ctx = serde_json::json!({
            "job_id": job_id,
            "message_type": format!("{:?}", claimed.headers.message_type),
            "depth": claimed.headers.depth,
        });
        let outcomes = hooks.dispatch(HookEvent::AfterClaim, &hook_ctx).await;
        for outcome in outcomes {
            if let HookOutcome::Inject(content) = outcome {
                initial_body = format!("{initial_body}\n\n---\n{content}");
            }
        }
    }

    // ── JIT retrieval: inject relevant long-term facts at claim start ──
    // Skip on continuations when scratchpad already has rich context (notes/subtasks)
    // to avoid re-injecting facts the agent already consolidated.
    let skip_jit = claimed.headers.message_type == MessageType::Continuation
        && (!scratchpad.notes.is_empty() || !scratchpad.subtasks.is_empty());
    if ctx.consolidation.retrieval_at_start && !skip_jit {
        let query = jit_retrieval::derive_query(&claimed.body, &scratchpad.goal);
        if !query.is_empty() {
            // Clone the estimator to avoid holding the mutex across the async store query.
            let est = ctx.estimator.lock().await.clone();
            let section = jit_retrieval::retrieve_facts_section(
                ctx.long_term.as_ref(),
                est,
                &query,
                ctx.consolidation.retrieval_max_facts,
                ctx.consolidation.retrieval_max_tokens,
            )
            .await;
            if !section.is_empty() {
                initial_body.push_str("\n\n---\n");
                initial_body.push_str(&section);
            }
        }
    }

    let messages = vec![ChatMessage::user_text(&initial_body)];

    let scratchpad_arc = Arc::new(Mutex::new(scratchpad));
    let orientation_provider: Option<Arc<dyn OrientationProvider>> = ctx
        .orientation_factory
        .as_ref()
        .map(|f| f.build(Arc::clone(&scratchpad_arc)));

    // Build per-claim tools via the factory
    let claim_ctx = ClaimContext {
        queue: Arc::clone(&ctx.queue),
        correlation_id: claimed.headers.correlation_id.clone(),
        current_depth: claimed.headers.depth,
        max_depth: ctx.limits.max_depth,
        parent_goal: extract_title(&claimed.body),
        parent_body: claimed.body.clone(),
        long_term: Arc::clone(&ctx.long_term),
        skills: Arc::clone(&ctx.skills),
        memory: Arc::clone(&ctx.memory),
        scratchpad: Arc::clone(&scratchpad_arc),
        hooks: ctx.hooks.clone(),
    };
    let tools = ctx.claim_tool_factory.build(&claim_ctx).await;

    let loop_config = LoopConfig {
        persona: &ctx.persona,
        llm: ctx.llm.as_ref(),
        compactor: ctx.compactor.as_ref(),
        tools: tools.as_ref(),
        estimator: &ctx.estimator,
        limits: ctx.limits.limits,
        scratchpad: Some(&scratchpad_arc),
        memory: Some(ctx.memory.as_ref()),
        cancel: None,
        timeout: None,
        compaction: ctx.compaction.clone(),
        hooks: ctx.hooks.as_deref(),
        scratch_dir: ctx.scratch_dir.clone(),
        orientation: orientation_provider.as_deref(),
    };
    let (turns, loop_outcome, final_messages) = run_agentic_loop(messages, &loop_config).await;

    // Best-effort: save full conversation for post-mortem debugging
    if let Some(ref scratch) = ctx.scratch_dir {
        let conv_dir = scratch.join("conversations");
        let _ = std::fs::create_dir_all(&conv_dir);
        let conv_path = conv_dir.join(format!("{job_id}.json"));
        let _ = std::fs::write(
            &conv_path,
            serde_json::to_string_pretty(&final_messages).unwrap_or_default(),
        );
    }

    // Persist calibration data after loop completes
    {
        let est = ctx.estimator.lock().await;
        let cal = est.calibration_data().clone();
        drop(est);
        let _ = ctx.long_term.persist_calibration(&cal).await;
    }
    let scratchpad = match Arc::try_unwrap(scratchpad_arc) {
        Ok(mutex) => mutex.into_inner(),
        Err(arc) => arc.lock().await.clone(),
    };

    // Map failure outcomes to Err; success outcomes fall through.
    let failure_reason = match &loop_outcome {
        LoopOutcome::LlmError(err) => Some(format!("LLM error: {err}")),
        LoopOutcome::TimedOut => Some("loop timeout exceeded".to_string()),
        LoopOutcome::Cancelled => Some("cancelled".to_string()),
        LoopOutcome::BlockedByHook(msg) => Some(format!("blocked by hook: {msg}")),
        LoopOutcome::ToolCallBudgetExhausted => Some("tool call budget exhausted".to_string()),
        LoopOutcome::Completed(_) | LoopOutcome::MaxTurnsExhausted => None,
    };
    if let Some(reason) = failure_reason {
        let record = AttemptRecord {
            attempt_number: claimed.headers.retry_count + 1,
            started_at,
            finished_at: now_unix_ts(),
            turns,
            final_text: None,
            outcome: AttemptOutcome::Failed,
            failure_reason: Some(reason.clone()),
        };
        return Err((record, reason, scratchpad));
    }

    // Extract final_text from Completed variant
    let final_text = match loop_outcome {
        LoopOutcome::Completed(text) => text,
        _ => None,
    };

    // Scratchpad is already up-to-date — tool calls went through the mutex.
    // Just persist the final state.
    let _ = ctx.memory.save(&scratchpad).await;

    let record = AttemptRecord {
        attempt_number: claimed.headers.retry_count + 1,
        started_at,
        finished_at: now_unix_ts(),
        turns,
        final_text,
        outcome: AttemptOutcome::Success,
        failure_reason: None,
    };

    // ── on_success hooks (fire before consolidation so hooks can block it) ──
    let mut skip_consolidation = false;
    if let Some(ref hooks) = ctx.hooks {
        let hook_ctx = serde_json::json!({
            "job_id": job_id,
            "turns": record.turns.len(),
            "final_text": record.final_text,
        });
        for outcome in hooks.dispatch(HookEvent::OnSuccess, &hook_ctx).await {
            if let HookOutcome::Block(_) = outcome {
                skip_consolidation = true;
                break;
            }
        }
    }

    // Best-effort consolidation (respects config + hook decisions)
    if ctx.consolidation.enabled && !skip_consolidation {
        consolidate_learnings(
            ctx.long_term.as_ref(),
            ctx.skills.as_ref(),
            ctx.skill_extractor.as_deref(),
            &claimed.headers.correlation_id,
            &record,
            &scratchpad,
            &ctx.consolidation,
            ctx.hooks.as_deref(),
        )
        .await;
    }

    Ok(record)
}

// ── Failure handling ────────────────────────────────────────────────

/// What to do after a failure is classified by hooks.
enum FailureAction {
    /// Non-retryable: move to failed/ immediately.
    Reject(String),
    /// Retry after a specified backoff (in seconds).
    RetryWithBackoff(u64),
    /// Generic failure: nack without delay, let the queue handle redelivery.
    RetryDefault,
}

#[allow(clippy::too_many_arguments)]
async fn handle_failure(
    queue: &dyn Queue,
    claimed: &ClaimedTask,
    record: &AttemptRecord,
    err: &str,
    scratchpad: &apex_core::domain::Scratchpad,
    worker_id: usize,
    composer: &MessageComposer,
    max_retries: u32,
    hooks: Option<&dyn HookRegistry>,
) -> Result<()> {
    dispatch_log(
        hooks,
        || {
            serde_json::json!({
                "level": "error",
                "event": "claim_failed",
                "worker_id": worker_id,
                "message_id": &claimed.id,
                "error": err,
            })
        },
        &format!("[worker {worker_id}] ✗ {} failed: {err}", claimed.id),
    )
    .await;

    let updated_body = composer.append_attempt_with_memory(&claimed.body, record, scratchpad);

    queue
        .update_body(claimed, &updated_body)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Let on_failure hooks classify the error; default to plain retry.
    let mut action = FailureAction::RetryDefault;
    if let Some(h) = hooks {
        let ctx = serde_json::json!({
            "error": err,
            "retry_count": claimed.headers.retry_count,
            "job_id": &claimed.headers.correlation_id,
        });
        for outcome in h.dispatch(HookEvent::OnFailure, &ctx).await {
            match outcome {
                HookOutcome::Block(reason) => {
                    action = FailureAction::Reject(reason);
                    break;
                }
                HookOutcome::Continue(Some(json_str)) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json_str) {
                        if let Some(reason) = v.get("block").and_then(|s| s.as_str()) {
                            action = FailureAction::Reject(reason.to_string());
                        } else if let Some(secs) = v.get("backoff_secs").and_then(|s| s.as_u64()) {
                            action = FailureAction::RetryWithBackoff(secs);
                        }
                    }
                    break;
                }
                _ => {} // Continue(None) = no opinion, continue to next hook
            }
        }
    }

    // Act on the decision
    match action {
        FailureAction::Reject(ref reason) => {
            queue
                .reject(claimed)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            dispatch_log(
                hooks,
                || {
                    serde_json::json!({
                        "level": "warn",
                        "event": "claim_rejected",
                        "worker_id": worker_id,
                        "message_id": &claimed.id,
                        "reason": reason,
                    })
                },
                &format!("[worker {worker_id}]   ↳ {reason}, moved to failed/"),
            )
            .await;
        }
        FailureAction::RetryWithBackoff(secs) => {
            dispatch_log(
                hooks,
                || {
                    serde_json::json!({
                        "level": "warn",
                        "event": "retry_backoff",
                        "worker_id": worker_id,
                        "message_id": &claimed.id,
                        "backoff_secs": secs,
                    })
                },
                &format!("[worker {worker_id}]   ↳ Backoff, delaying {secs}s before retry"),
            )
            .await;
            queue
                .nack_with_delay(claimed, std::time::Duration::from_secs(secs))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        FailureAction::RetryDefault => {
            queue
                .nack(claimed)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }

    if !matches!(action, FailureAction::Reject(_)) {
        if claimed.headers.retry_count + 1 >= max_retries {
            dispatch_log(
                hooks,
                || {
                    serde_json::json!({
                        "level": "warn",
                        "event": "max_retries_reached",
                        "worker_id": worker_id,
                        "message_id": &claimed.id,
                    })
                },
                &format!("[worker {worker_id}]   ↳ Max retries reached, moved to failed/"),
            )
            .await;
        } else {
            dispatch_log(
                hooks,
                || {
                    serde_json::json!({
                        "level": "info",
                        "event": "retry_scheduled",
                        "worker_id": worker_id,
                        "message_id": &claimed.id,
                        "attempt": claimed.headers.retry_count + 2,
                        "max_retries": max_retries,
                    })
                },
                &format!(
                    "[worker {worker_id}]   ↳ Requeued for retry (attempt {} of {})",
                    claimed.headers.retry_count + 2,
                    max_retries
                ),
            )
            .await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_mocks::*;
    use apex_core::domain::QueueDepth;

    fn make_claimed(id: &str, retry_count: u32) -> ClaimedTask {
        ClaimedTask {
            id: id.to_string(),
            claim_path: format!("/tmp/test/{id}"),
            headers: apex_core::domain::MessageHeaders {
                message_type: MessageType::Task,
                correlation_id: "corr-1".to_string(),
                depth: 0,
                retry_count,
                depends_on: vec![],
                skills: vec![],
            },
            body: "test body".to_string(),
        }
    }

    fn make_record() -> AttemptRecord {
        AttemptRecord {
            attempt_number: 1,
            started_at: "0".into(),
            finished_at: "1".into(),
            turns: vec![],
            final_text: None,
            outcome: AttemptOutcome::Failed,
            failure_reason: Some("test error".into()),
        }
    }

    // Without hooks, all errors default to plain nack (RetryDefault)
    #[tokio::test]
    async fn handle_failure_no_hooks_nacks() {
        let queue = MockQueue::new();
        let claimed = make_claimed("task-1", 0);
        let record = make_record();
        let scratchpad = apex_core::domain::Scratchpad::new("j1", "goal");
        let composer = MessageComposer::default();

        handle_failure(
            &queue,
            &claimed,
            &record,
            "authentication_error: bad token",
            &scratchpad,
            0,
            &composer,
            3,
            None,
        )
        .await
        .unwrap();

        let actions = queue.actions.lock().await;
        // Without hooks, everything defaults to nack
        assert!(
            actions.iter().any(|a| matches!(a, QueueAction::Nack(_))),
            "expected Nack action, got: {:?}",
            actions
        );
    }

    #[tokio::test]
    async fn handle_failure_generic_nacks() {
        let queue = MockQueue::new();
        let claimed = make_claimed("task-3", 0);
        let record = make_record();
        let scratchpad = apex_core::domain::Scratchpad::new("j3", "goal");
        let composer = MessageComposer::default();

        handle_failure(
            &queue,
            &claimed,
            &record,
            "connection timeout",
            &scratchpad,
            0,
            &composer,
            3,
            None,
        )
        .await
        .unwrap();

        let actions = queue.actions.lock().await;
        assert!(
            actions.iter().any(|a| matches!(a, QueueAction::Nack(_))),
            "expected Nack action, got: {:?}",
            actions
        );
        assert!(!actions.iter().any(|a| matches!(a, QueueAction::Reject(_))));
        assert!(!actions
            .iter()
            .any(|a| matches!(a, QueueAction::NackWithDelay(_, _))));
    }

    #[tokio::test]
    async fn worker_exits_on_empty_queue() {
        let queue = Arc::new(MockQueue::new());
        *queue.depth_override.lock().await = Some(QueueDepth {
            pending: 0,
            processing: 0,
        });

        let claim_factory: Arc<dyn ClaimToolFactory> = Arc::new(MockClaimToolFactory);
        let llm: Arc<dyn LlmProvider> = Arc::new(MockLlmProvider::text_only("unused"));
        let memory: Arc<dyn WorkingMemory> = Arc::new(MockWorkingMemory::new());
        let long_term: Arc<dyn MemoryStore> = Arc::new(MockMemoryStore::new());
        let skills: Arc<dyn SkillStore> = Arc::new(MockSkillStore::new());

        let compactor: Arc<dyn ConversationCompactor> =
            Arc::new(MockConversationCompactor::new("mock summary"));
        let ctx = WorkerContext {
            queue: queue as Arc<dyn Queue>,
            claim_tool_factory: claim_factory,
            llm,
            compactor,
            skill_extractor: None,
            memory,
            long_term,
            skills,
            persona: Arc::new("test persona".to_string()),
            limits: WorkerLimits {
                max_depth: 3,
                max_retries: 3,
                max_empty_cycles: 300,
                limits: LoopLimits {
                    max_tool_result_bytes: 10_000,
                    max_output_tokens: 4096,
                    reserved_reasoning_tokens: 4096,
                    max_turns: 32,
                    max_tool_input_bytes: 40_000,
                    max_tool_calls_per_turn: 64,
                    max_total_tool_calls: 512,
                    prompt_caching: true,
                },
            },
            estimator: Arc::new(Mutex::new(TokenEstimator::default())),
            compaction: CompactionSection {
                preserve_turns: 3,
                max_summary_tokens: 1024,
                spill_history: false,
            },
            consolidation: ConsolidationSection::default(),
            hooks: None,
            scratch_dir: None,
            orientation_factory: None,
        };

        let result = worker_loop(ctx, 0).await;
        assert!(result.is_ok(), "worker should exit cleanly: {:?}", result);
    }
}
