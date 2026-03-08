mod queue_tools;

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

use apex_context::MessageComposer;
use apex_core::domain::{
    AttemptOutcome, AttemptRecord, ChatMessage, ClaimedTask, CompletionRequest, ContentBlock,
    MessageHeaders, MessageRole, MessageType, QueueMessage, ToolCall, ToolCallRecord, ToolDef,
    ToolResult, TurnRecord,
};
use apex_core::error::ToolError;
use apex_core::ports::{LlmProvider, Queue, ToolRegistry, WorkingMemory};
use apex_llm::anthropic::AnthropicProvider;
use apex_memory::{FsScratchpadStore, MemoryToolRegistry};
use apex_queue::RfbmqAdapter;
use apex_tools::BuiltinToolRegistry;

use crate::queue_tools::QueueToolRegistry;

const MAX_TURNS: usize = 32;
const MAX_TOKENS: u32 = 8192;
const MAX_RETRIES: u32 = 3;
const DEFAULT_MAX_DEPTH: u32 = 3;

// ── CompositeToolRegistry ─────────────────────────────────────────

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

// ── Path resolution ────────────────────────────────────────────────

struct ApexPaths {
    root: PathBuf,
    prompts_dir: PathBuf,
    queue_dir: PathBuf,
    memory_dir: PathBuf,
}

impl ApexPaths {
    fn resolve() -> Result<Self> {
        let root = std::env::var("APEX_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        Ok(Self {
            prompts_dir: root.join("prompts"),
            queue_dir: root.join("queues").join("work"),
            memory_dir: root.join("memory").join("working"),
            root,
        })
    }
}

// ── CLI ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("init") => cmd_init().await,
        Some("run") => {
            let task = if args.len() > 1 {
                args[1..].join(" ")
            } else {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .context("failed to read task from stdin")?;
                buf
            };

            let task = task.trim().to_string();
            if task.is_empty() {
                bail!("no task provided. Usage: apex run \"<task>\"");
            }

            cmd_run(task).await
        }
        Some("queue") => {
            let subcmd = args.get(1).map(|s| s.as_str());
            match subcmd {
                Some("reap") => cmd_queue_reap().await,
                None => cmd_queue_depth().await,
                Some(sub) => bail!("unknown queue subcommand: {sub}. Available: reap"),
            }
        }
        Some("cat") => {
            let path = args.get(1).context("Usage: apex cat <message-path>")?;
            cmd_cat(path).await
        }
        Some("work") => cmd_work().await,
        Some("status") => cmd_status().await,
        Some(cmd) => bail!(
            "unknown command: {cmd}. Available: init, run, queue, cat, work, status"
        ),
        None => bail!("no command provided. Usage: apex <command>"),
    }
}

// ── Commands ───────────────────────────────────────────────────────

async fn cmd_init() -> Result<()> {
    let paths = ApexPaths::resolve()?;

    std::fs::create_dir_all(paths.queue_dir.parent().unwrap())
        .context("failed to create queues/ directory")?;

    std::fs::create_dir_all(&paths.memory_dir)
        .context("failed to create memory/working/ directory")?;

    RfbmqAdapter::init(&paths.queue_dir).map_err(|e| anyhow::anyhow!("{e}"))?;

    eprintln!("✓ Initialized apex at {}", paths.root.display());
    eprintln!("  queue:  {}", paths.queue_dir.display());
    eprintln!("  memory: {}", paths.memory_dir.display());
    Ok(())
}

async fn cmd_run(task: String) -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let adapter = open_queue(&paths)?;

    let correlation_id = format!("job-{}", &uuid_v4()[..8]);
    let body = MessageComposer::compose_task_body(&task);

    let msg = QueueMessage {
        headers: MessageHeaders {
            message_type: MessageType::Goal,
            correlation_id: correlation_id.clone(),
            depth: 0,
            retry_count: 0,
            depends_on: vec![],
        },
        body,
    };

    let id = adapter.push(msg).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    eprintln!("▶ Queued goal {id} (correlation: {correlation_id})");

    // Process the queue until done
    process_queue(&paths, Arc::new(adapter)).await
}

async fn cmd_work() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let adapter = open_queue(&paths)?;
    process_queue(&paths, Arc::new(adapter)).await
}

async fn cmd_queue_depth() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let adapter = open_queue(&paths)?;

    let d = adapter
        .depth()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("{}", d.pending + d.processing);
    Ok(())
}

async fn cmd_queue_reap() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let adapter = open_queue(&paths)?;

    let result = adapter
        .reap()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    eprintln!("reaped {} lease(s)", result.lease_reaped);
    Ok(())
}

async fn cmd_cat(path: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read message file: {path}"))?;

    // rfbmq format: headers separated from body by a blank line
    if let Some(pos) = content.find("\n\n") {
        println!("{}", &content[pos + 2..]);
    } else {
        println!("{content}");
    }
    Ok(())
}

async fn cmd_status() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let queue_root = &paths.queue_dir;

    if !queue_root.exists() {
        bail!("queue not found. Run 'apex init' first.");
    }

    let dirs = ["pending", "processing", "done", "failed"];
    for dir_name in &dirs {
        let dir = queue_root.join(dir_name);
        if !dir.exists() {
            continue;
        }

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("failed to read {dir_name}/ directory"))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    == Some("md")
            })
            .collect();

        if entries.is_empty() {
            continue;
        }

        println!("── {dir_name}/ ({} messages) ──", entries.len());
        for entry in &entries {
            let path = entry.path();
            match rfbmq_core::Message::from_file(&path) {
                Ok(msg) => {
                    let id = msg
                        .header
                        .id
                        .as_ref()
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "???".to_string());
                    let corr = msg
                        .header
                        .correlation_id
                        .as_deref()
                        .unwrap_or("-");
                    let msg_type = msg
                        .header
                        .custom
                        .iter()
                        .find(|l| l.starts_with("Type:"))
                        .map(|l| l.trim_start_matches("Type:").trim())
                        .unwrap_or("unknown");
                    let deps = if msg.header.depends_on.is_empty() {
                        String::new()
                    } else {
                        let dep_strs: Vec<&str> =
                            msg.header.depends_on.iter().map(|d| d.as_str()).collect();
                        format!(" depends_on=[{}]", dep_strs.join(", "))
                    };
                    let short_id = &id[..id.len().min(12)];
                    println!("  {short_id}  {msg_type:<13} corr={corr}{deps}");
                }
                Err(e) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    println!("  {name}  (parse error: {e})");
                }
            }
        }
    }

    Ok(())
}

// ── Queue helpers ──────────────────────────────────────────────────

fn open_queue(paths: &ApexPaths) -> Result<RfbmqAdapter> {
    RfbmqAdapter::open(&paths.queue_dir).map_err(|e| {
        anyhow::anyhow!(
            "failed to open queue at {}. Run 'apex init' first. Error: {e}",
            paths.queue_dir.display()
        )
    })
}

// ── Queue processing loop ──────────────────────────────────────────

async fn process_queue(paths: &ApexPaths, adapter: Arc<RfbmqAdapter>) -> Result<()> {
    let persona_path = paths.prompts_dir.join("agent.md");
    let persona =
        std::fs::read_to_string(&persona_path).context("failed to read prompts/agent.md")?;

    let max_concurrent: usize = std::env::var("APEX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let max_depth: u32 = std::env::var("APEX_MAX_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_DEPTH);

    let llm = Arc::new(AnthropicProvider::from_env());
    let memory: Arc<dyn WorkingMemory> =
        Arc::new(FsScratchpadStore::new(paths.memory_dir.clone()));
    let queue: Arc<dyn Queue> = adapter.clone();

    let persona = Arc::new(persona);
    let memory = Arc::clone(&memory);

    if max_concurrent <= 1 {
        // Single worker — no spawning needed
        worker_loop(
            adapter,
            queue,
            llm,
            memory,
            persona,
            max_depth,
            0,
        )
        .await
    } else {
        let mut handles = Vec::new();
        for worker_id in 0..max_concurrent {
            let adapter = Arc::clone(&adapter);
            let queue = queue.clone();
            let llm = Arc::clone(&llm);
            let memory = Arc::clone(&memory);
            let persona = Arc::clone(&persona);

            handles.push(tokio::spawn(async move {
                worker_loop(adapter, queue, llm, memory, persona, max_depth, worker_id).await
            }));
        }

        // Wait for all workers to finish
        for handle in handles {
            handle.await??;
        }
        Ok(())
    }
}

async fn worker_loop(
    adapter: Arc<RfbmqAdapter>,
    queue: Arc<dyn Queue>,
    llm: Arc<AnthropicProvider>,
    memory: Arc<dyn WorkingMemory>,
    persona: Arc<String>,
    max_depth: u32,
    worker_id: usize,
) -> Result<()> {
    let mut empty_cycles = 0u32;

    loop {
        let claimed = adapter
            .pop()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let claimed = match claimed {
            Some(c) => {
                empty_cycles = 0;
                c
            }
            None => {
                // Check if queue is truly empty or just blocked on deps
                let depth = adapter
                    .depth()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;

                if depth.pending + depth.processing == 0 {
                    // Queue is truly empty — exit
                    return Ok(());
                }

                // Messages exist but deps unsatisfied — check for failed deps
                check_failed_deps(&adapter).await?;

                // Sleep and retry
                empty_cycles += 1;
                if empty_cycles > 300 {
                    // Safety: don't spin forever (5 min at 1s intervals)
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

        // Build per-task tool registry with queue tools
        let queue_tools = QueueToolRegistry::new(
            Arc::clone(&queue),
            claimed.headers.correlation_id.clone(),
            claimed.headers.depth,
            max_depth,
            extract_title(&claimed.body),
            claimed.body.clone(),
        );

        let memory_tools = MemoryToolRegistry::new(Arc::clone(&memory));
        let tools = CompositeToolRegistry::new(vec![
            Box::new(BuiltinToolRegistry::new()),
            Box::new(memory_tools),
            Box::new(queue_tools),
        ]);

        match claimed.headers.message_type {
            MessageType::Goal | MessageType::Task | MessageType::Subtask => {
                match execute_task(&claimed, &persona, llm.as_ref(), &tools, memory.as_ref()).await
                {
                    Ok(record) => {
                        let title = extract_title(&claimed.body);
                        let result_body = MessageComposer::compose_result(&title, &record);
                        adapter
                            .update_body(&claimed, &result_body)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        adapter
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
                            &adapter, &claimed, &record, &err, &scratchpad, worker_id,
                        )
                        .await?;
                    }
                }
            }
            MessageType::Continuation => {
                match execute_continuation(
                    &claimed,
                    &persona,
                    llm.as_ref(),
                    &tools,
                    memory.as_ref(),
                    queue.as_ref(),
                )
                .await
                {
                    Ok(record) => {
                        let title = extract_title(&claimed.body);
                        let result_body = MessageComposer::compose_result(&title, &record);
                        adapter
                            .update_body(&claimed, &result_body)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        adapter
                            .ack(&claimed)
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        eprintln!(
                            "[worker {worker_id}] ✓ Continuation {} completed",
                            claimed.id
                        );
                    }
                    Err((record, err, scratchpad)) => {
                        handle_failure(
                            &adapter, &claimed, &record, &err, &scratchpad, worker_id,
                        )
                        .await?;
                    }
                }
            }
        }
    }
}

async fn handle_failure(
    adapter: &RfbmqAdapter,
    claimed: &ClaimedTask,
    record: &AttemptRecord,
    err: &str,
    scratchpad: &apex_core::domain::Scratchpad,
    worker_id: usize,
) -> Result<()> {
    eprintln!("[worker {worker_id}] ✗ {} failed: {err}", claimed.id);

    let updated_body =
        MessageComposer::append_attempt_with_memory(&claimed.body, record, scratchpad);

    adapter
        .update_body(claimed, &updated_body)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    adapter
        .nack(claimed)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if claimed.headers.retry_count + 1 >= MAX_RETRIES {
        eprintln!("[worker {worker_id}]   ↳ Max retries reached, message moved to failed/");
    } else {
        eprintln!(
            "[worker {worker_id}]   ↳ Requeued for retry (attempt {} of {})",
            claimed.headers.retry_count + 2,
            MAX_RETRIES
        );
    }
    Ok(())
}

/// Check if any pending messages have dependencies in failed/.
/// If so, fail those messages too to prevent deadlocks.
async fn check_failed_deps(adapter: &RfbmqAdapter) -> Result<()> {
    // This is a best-effort check. We scan pending messages and check
    // if any of their dependencies are in failed/. If so, we pop and nack them.
    // For now this is a simple implementation; a more robust version would
    // directly scan the filesystem.
    // TODO: Implement proper failed dependency cascade
    let _ = adapter;
    Ok(())
}

// ── Agent loop (multi-turn LLM + tool execution) ───────────────────

async fn execute_task(
    claimed: &ClaimedTask,
    persona: &str,
    llm: &dyn LlmProvider,
    tools: &dyn ToolRegistry,
    memory: &dyn WorkingMemory,
) -> std::result::Result<AttemptRecord, (AttemptRecord, String, apex_core::domain::Scratchpad)> {
    let started_at = now_iso();
    let schemas = tools.schemas();

    // Load or create scratchpad for this job
    let job_id = &claimed.headers.correlation_id;
    let mut scratchpad = memory
        .load_or_create(job_id)
        .await
        .unwrap_or_else(|_| apex_core::domain::Scratchpad::new(job_id, ""));

    // Set goal from task title if empty
    if scratchpad.goal.is_empty() {
        scratchpad.goal = extract_title(&claimed.body);
        let _ = memory.save(&scratchpad).await;
    }

    // Build initial message, injecting working memory if it has content
    let initial_body = if !scratchpad.subtasks.is_empty() || !scratchpad.notes.is_empty() {
        format!(
            "{}\n\n---\n## Working Memory (from previous iterations)\n{}",
            claimed.body,
            scratchpad.to_markdown()
        )
    } else {
        claimed.body.clone()
    };

    let mut messages = vec![ChatMessage::user_text(&initial_body)];
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut final_text: Option<String> = None;

    for turn_num in 0..MAX_TURNS {
        let req = CompletionRequest {
            system_prompt: persona.to_string(),
            messages: messages.clone(),
            max_tokens: MAX_TOKENS,
            temperature: Some(0.2),
        };

        let resp = match llm.complete_with_tools(req, &schemas).await {
            Ok(r) => r,
            Err(err) => {
                let record = AttemptRecord {
                    attempt_number: claimed.headers.retry_count + 1,
                    started_at,
                    finished_at: now_iso(),
                    turns,
                    final_text: None,
                    outcome: AttemptOutcome::Failed,
                    failure_reason: Some(format!("LLM error: {err}")),
                    eval_summary: None,
                };
                return Err((record, format!("LLM error: {err}"), scratchpad));
            }
        };

        eprintln!(
            "  turn {}: {} tool call(s), {} input / {} output tokens",
            turn_num + 1,
            resp.tool_calls.len(),
            resp.usage.input_tokens,
            resp.usage.output_tokens,
        );

        messages.push(resp.message.clone());

        // If no tool calls, this is the final response
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

        // Execute tool calls and record results
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

    // Save scratchpad after successful execution
    if let Ok(updated_pad) = memory.load_or_create(job_id).await {
        scratchpad = updated_pad;
    }
    let _ = memory.save(&scratchpad).await;

    // Step 7: Deterministic evaluation
    let eval_summary = match apex_eval::Evaluator::run_deterministic(&claimed.body).await {
        Some(eval_result) if !eval_result.all_passed() => {
            let summary = eval_result.failure_summary();
            eprintln!("  eval: {}/{} checks failed", eval_result.failed, eval_result.total);
            let record = AttemptRecord {
                attempt_number: claimed.headers.retry_count + 1,
                started_at,
                finished_at: now_iso(),
                turns,
                final_text,
                outcome: AttemptOutcome::Failed,
                failure_reason: Some("deterministic evaluation failed".into()),
                eval_summary: Some(summary),
            };
            return Err((record, "deterministic evaluation failed".into(), scratchpad));
        }
        Some(eval_result) => {
            eprintln!("  eval: {}/{} checks passed", eval_result.passed, eval_result.total);
            Some(eval_result.full_summary())
        }
        None => None,
    };

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

    Ok(record)
}

// ── Continuation handler ──────────────────────────────────────────

async fn execute_continuation(
    claimed: &ClaimedTask,
    persona: &str,
    llm: &dyn LlmProvider,
    tools: &dyn ToolRegistry,
    memory: &dyn WorkingMemory,
    queue: &dyn Queue,
) -> std::result::Result<AttemptRecord, (AttemptRecord, String, apex_core::domain::Scratchpad)> {
    let started_at = now_iso();
    let job_id = &claimed.headers.correlation_id;

    let scratchpad = memory
        .load_or_create(job_id)
        .await
        .unwrap_or_else(|_| apex_core::domain::Scratchpad::new(job_id, ""));

    // Read all done subtask results for this correlation ID
    let done_ids = queue
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
        if let Ok(body) = queue.read_done_body(id).await {
            subtask_results.push((id.clone(), body));
        }
    }

    // Use the LLM to assemble a coherent summary via the tool loop
    // The continuation body already instructs the agent to use queue_read_done
    let schemas = tools.schemas();

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

    let mut messages = vec![ChatMessage::user_text(&initial_body)];
    let mut turns: Vec<TurnRecord> = Vec::new();
    let mut final_text: Option<String> = None;

    for turn_num in 0..MAX_TURNS {
        let req = CompletionRequest {
            system_prompt: persona.to_string(),
            messages: messages.clone(),
            max_tokens: MAX_TOKENS,
            temperature: Some(0.2),
        };

        let resp = match llm.complete_with_tools(req, &schemas).await {
            Ok(r) => r,
            Err(err) => {
                let record = AttemptRecord {
                    attempt_number: claimed.headers.retry_count + 1,
                    started_at,
                    finished_at: now_iso(),
                    turns,
                    final_text: None,
                    outcome: AttemptOutcome::Failed,
                    failure_reason: Some(format!("LLM error: {err}")),
                    eval_summary: None,
                };
                return Err((record, format!("LLM error: {err}"), scratchpad));
            }
        };

        eprintln!(
            "  turn {}: {} tool call(s), {} input / {} output tokens",
            turn_num + 1,
            resp.tool_calls.len(),
            resp.usage.input_tokens,
            resp.usage.output_tokens,
        );

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

    // Deterministic evaluation for continuation
    let eval_summary = match apex_eval::Evaluator::run_deterministic(&claimed.body).await {
        Some(eval_result) if !eval_result.all_passed() => {
            let summary = eval_result.failure_summary();
            eprintln!("  eval: {}/{} checks failed", eval_result.failed, eval_result.total);
            let record = AttemptRecord {
                attempt_number: claimed.headers.retry_count + 1,
                started_at,
                finished_at: now_iso(),
                turns,
                final_text,
                outcome: AttemptOutcome::Failed,
                failure_reason: Some("deterministic evaluation failed".into()),
                eval_summary: Some(summary),
            };
            return Err((record, "deterministic evaluation failed".into(), scratchpad));
        }
        Some(eval_result) => {
            eprintln!("  eval: {}/{} checks passed", eval_result.passed, eval_result.total);
            Some(eval_result.full_summary())
        }
        None => None,
    };

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

    Ok(record)
}

// ── Utilities ──────────────────────────────────────────────────────

fn extract_title(body: &str) -> String {
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

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{:016x}{:08x}", t, pid)
}
