mod queue_tools;

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use apex_context::{MessageComposer, TokenEstimator};
use apex_core::domain::{
    AttemptOutcome, AttemptRecord, ChatMessage, ClaimedTask, CompletionRequest,
    ContentBlock, MessageHeaders, MessageRole, MessageType, QueueMessage, ToolCall, ToolCallRecord,
    ToolDef, ToolResult, TurnRecord,
};
use apex_core::error::ToolError;
use apex_core::domain::{Fact, FactId, Skill, SkillId, Strategy, StrategyId};
use apex_core::ports::{LlmProvider, MemoryStore, Queue, ToolRegistry, WorkingMemory};
use apex_llm::anthropic::AnthropicProvider;
use apex_memory::{FsScratchpadStore, LongTermMemoryToolRegistry, MemoryToolRegistry, SqliteMemoryStore};
use apex_queue::RfbmqAdapter;
use apex_config::{ConfigLoader, Invariants, validate_full};
use apex_tools::BuiltinToolRegistry;
use apex_tools::ConfigToolRegistry;
use apex_tools::CustomToolRegistry;
use apex_tools::spill::SpillManager;

use crate::queue_tools::QueueToolRegistry;

const MAX_TURNS: usize = 32;
const MAX_TOKENS: u32 = 8192;

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
    long_term_memory_dir: PathBuf,
    scratch_dir: PathBuf,
    tools_dir: PathBuf,
    config_dir: PathBuf,
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
            long_term_memory_dir: root.join("memory").join("long-term"),
            scratch_dir: root.join("scratch"),
            tools_dir: root.join("tools"),
            config_dir: root.join("config"),
            root,
        })
    }

    fn long_term_db_path(&self) -> PathBuf {
        self.long_term_memory_dir.join("memory.db")
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
        Some("memory") => {
            let subcmd = args.get(1).map(|s| s.as_str());
            match subcmd {
                Some("facts") => cmd_memory_facts().await,
                Some("skills") => cmd_memory_skills().await,
                Some("strategies") => cmd_memory_strategies().await,
                Some("calibration") => cmd_memory_calibration().await,
                None => {
                    cmd_memory_facts().await?;
                    cmd_memory_skills().await?;
                    cmd_memory_strategies().await
                }
                Some(sub) => bail!("unknown memory subcommand: {sub}. Available: facts, skills, strategies, calibration"),
            }
        }
        Some("scratch") => {
            let subcmd = args.get(1).map(|s| s.as_str());
            match subcmd {
                Some("ls") | None => cmd_scratch_ls().await,
                Some(sub) => bail!("unknown scratch subcommand: {sub}. Available: ls"),
            }
        }
        Some("tools") => {
            let subcmd = args.get(1).map(|s| s.as_str());
            match subcmd {
                Some("list") | None => cmd_tools_list().await,
                Some(sub) => bail!("unknown tools subcommand: {sub}. Available: list"),
            }
        }
        Some("config") => {
            let subcmd = args.get(1).map(|s| s.as_str());
            match subcmd {
                Some("show") | None => cmd_config_show().await,
                Some("invariants") => cmd_config_invariants().await,
                Some(sub) => bail!("unknown config subcommand: {sub}. Available: show, invariants"),
            }
        }
        Some("validate") => cmd_validate().await,
        Some(cmd) => bail!(
            "unknown command: {cmd}. Available: init, run, queue, cat, work, status, memory, scratch, tools, config, validate"
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

    std::fs::create_dir_all(&paths.long_term_memory_dir)
        .context("failed to create memory/long-term/ directory")?;

    std::fs::create_dir_all(&paths.scratch_dir)
        .context("failed to create scratch/ directory")?;

    std::fs::create_dir_all(paths.tools_dir.join("custom"))
        .context("failed to create tools/custom/ directory")?;

    std::fs::create_dir_all(&paths.config_dir)
        .context("failed to create config/ directory")?;

    // Write empty manifest if absent
    let manifest_path = paths.tools_dir.join("manifest.toml");
    if !manifest_path.exists() {
        std::fs::write(&manifest_path, "")
            .context("failed to write tools/manifest.toml")?;
    }

    // Write default config files if absent
    ConfigLoader::write_default_invariants(&paths.config_dir)?;
    ConfigLoader::write_default_agent_config(&paths.config_dir)?;

    RfbmqAdapter::init(&paths.queue_dir).map_err(|e| anyhow::anyhow!("{e}"))?;

    eprintln!("✓ Initialized apex at {}", paths.root.display());
    eprintln!("  queue:    {}", paths.queue_dir.display());
    eprintln!("  memory:   {}", paths.memory_dir.display());
    eprintln!("  long-term: {}", paths.long_term_memory_dir.display());
    eprintln!("  scratch:  {}", paths.scratch_dir.display());
    eprintln!("  tools:    {}", paths.tools_dir.display());
    eprintln!("  config:   {}", paths.config_dir.display());
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

    // Clean scratch files after reap
    let spill = SpillManager::new(paths.scratch_dir);
    match spill.clean_all() {
        Ok(n) if n > 0 => eprintln!("cleaned {n} scratch file(s)"),
        _ => {}
    }

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

    let evaluator_persona_path = paths.prompts_dir.join("evaluator.md");
    let evaluator_persona = std::fs::read_to_string(&evaluator_persona_path)
        .context("failed to read prompts/evaluator.md")?;

    let invariants = ConfigLoader::load_invariants(&paths.config_dir)?;
    let agent_config = ConfigLoader::load_agent_config(&paths.config_dir)?;

    let max_concurrent = agent_config.agent.max_concurrent;
    let max_depth = agent_config.agent.max_depth;
    let max_retries = agent_config.agent.max_retries;

    let eval_config = apex_eval::EvalConfig {
        eval_model: agent_config.eval.eval_model.clone(),
        eval_on: match agent_config.eval.eval_on.as_str() {
            "always" => apex_eval::EvalOn::Always,
            "never" => apex_eval::EvalOn::Never,
            _ => apex_eval::EvalOn::FuzzyCriteria,
        },
    };

    let llm = Arc::new(AnthropicProvider::from_env());

    // Create a separate LLM provider for eval if a different model is configured
    let eval_llm: Arc<dyn LlmProvider> = if let Some(ref eval_model) = eval_config.eval_model {
        let api_key =
            std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set");
        Arc::new(AnthropicProvider::new(api_key, eval_model.clone(), 200_000))
    } else {
        Arc::clone(&llm) as Arc<dyn LlmProvider>
    };

    let memory: Arc<dyn WorkingMemory> =
        Arc::new(FsScratchpadStore::new(paths.memory_dir.clone()));
    let queue: Arc<dyn Queue> = adapter.clone();

    let long_term: Arc<dyn MemoryStore> = Arc::new(
        SqliteMemoryStore::open(&paths.long_term_db_path())
            .context("failed to open long-term memory database")?,
    );

    // Load calibration data for token estimation
    let calibration = long_term.load_calibration().await.unwrap_or_default();
    let estimator = Arc::new(Mutex::new(TokenEstimator::new(calibration)));

    let scratch_dir = paths.scratch_dir.clone();
    let tools_dir = paths.tools_dir.clone();
    let config_dir = paths.config_dir.clone();

    let persona = Arc::new(persona);
    let evaluator_persona = Arc::new(evaluator_persona);
    let eval_config = Arc::new(eval_config);
    let memory = Arc::clone(&memory);
    let invariants = Arc::new(invariants);

    if max_concurrent <= 1 {
        // Single worker — no spawning needed
        worker_loop(
            adapter,
            queue,
            llm,
            eval_llm,
            memory,
            long_term,
            persona,
            evaluator_persona,
            eval_config,
            max_depth,
            max_retries,
            0,
            scratch_dir,
            tools_dir,
            config_dir,
            invariants,
            estimator,
        )
        .await
    } else {
        let mut handles = Vec::new();
        for worker_id in 0..max_concurrent {
            let adapter = Arc::clone(&adapter);
            let queue = queue.clone();
            let llm = Arc::clone(&llm);
            let eval_llm = Arc::clone(&eval_llm);
            let memory = Arc::clone(&memory);
            let long_term = Arc::clone(&long_term);
            let persona = Arc::clone(&persona);
            let evaluator_persona = Arc::clone(&evaluator_persona);
            let eval_config = Arc::clone(&eval_config);
            let scratch_dir = scratch_dir.clone();
            let tools_dir = tools_dir.clone();
            let config_dir = config_dir.clone();
            let invariants = Arc::clone(&invariants);
            let estimator = Arc::clone(&estimator);

            handles.push(tokio::spawn(async move {
                worker_loop(
                    adapter,
                    queue,
                    llm,
                    eval_llm,
                    memory,
                    long_term,
                    persona,
                    evaluator_persona,
                    eval_config,
                    max_depth,
                    max_retries,
                    worker_id,
                    scratch_dir,
                    tools_dir,
                    config_dir,
                    invariants,
                    estimator,
                )
                .await
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
    eval_llm: Arc<dyn LlmProvider>,
    memory: Arc<dyn WorkingMemory>,
    long_term: Arc<dyn MemoryStore>,
    persona: Arc<String>,
    evaluator_persona: Arc<String>,
    eval_config: Arc<apex_eval::EvalConfig>,
    max_depth: u32,
    max_retries: u32,
    worker_id: usize,
    scratch_dir: PathBuf,
    tools_dir: PathBuf,
    config_dir: PathBuf,
    invariants: Arc<Invariants>,
    estimator: Arc<Mutex<TokenEstimator>>,
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
            Some(Arc::clone(&long_term)),
        );

        let memory_tools = MemoryToolRegistry::new(Arc::clone(&memory));
        let lt_memory_tools = LongTermMemoryToolRegistry::new(Arc::clone(&long_term));
        let custom_spill = SpillManager::new(scratch_dir.clone());
        let custom_tools = CustomToolRegistry::new(
            tools_dir.clone(),
            custom_spill,
            Some(Arc::clone(&long_term)),
        );
        let config_tools = ConfigToolRegistry::new(
            config_dir.clone(),
            Arc::clone(&invariants),
        );
        let tools = CompositeToolRegistry::new(vec![
            Box::new(BuiltinToolRegistry::new(scratch_dir.clone())),
            Box::new(memory_tools),
            Box::new(lt_memory_tools),
            Box::new(queue_tools),
            Box::new(custom_tools),
            Box::new(config_tools),
        ]);

        // Build composer with current calibration
        let composer = {
            let est = estimator.lock().await;
            MessageComposer::new(TokenEstimator::new(est.calibration_data().clone()))
        };

        match claimed.headers.message_type {
            MessageType::Goal | MessageType::Task | MessageType::Subtask => {
                match execute_task(
                    &claimed,
                    &persona,
                    llm.as_ref(),
                    &tools,
                    memory.as_ref(),
                    long_term.as_ref(),
                    eval_llm.as_ref(),
                    &eval_config,
                    &evaluator_persona,
                    &estimator,
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
                            "[worker {worker_id}] ✓ {type_label} {} completed",
                            claimed.id
                        );
                    }
                    Err((record, err, scratchpad)) => {
                        handle_failure(
                            &adapter, &claimed, &record, &err, &scratchpad, worker_id, &composer, max_retries,
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
                    long_term.as_ref(),
                    eval_llm.as_ref(),
                    &eval_config,
                    &evaluator_persona,
                    &estimator,
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
                            &adapter, &claimed, &record, &err, &scratchpad, worker_id, &composer, max_retries,
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
    long_term: &dyn MemoryStore,
    eval_llm: &dyn LlmProvider,
    eval_config: &apex_eval::EvalConfig,
    evaluator_persona: &str,
    estimator: &Arc<Mutex<TokenEstimator>>,
) -> std::result::Result<AttemptRecord, (AttemptRecord, String, apex_core::domain::Scratchpad)> {
    let started_at = now_iso();

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
        // Refresh schemas each turn so newly created tools appear immediately
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

        // Calibrate token estimator from actual usage
        {
            let prompt_text: String = messages
                .iter()
                .map(|m| m.text())
                .collect::<Vec<_>>()
                .join("\n");
            let mut est = estimator.lock().await;
            est.calibrate(&prompt_text, resp.usage.input_tokens);
            // Persist every 5 samples
            if est.calibration_data().sample_count % 5 == 0 {
                let cal = est.calibration_data().clone();
                drop(est);
                let _ = long_term.persist_calibration(&cal).await;
            }
        }

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

    // Save scratchpad after successful execution
    if let Ok(updated_pad) = memory.load_or_create(job_id).await {
        scratchpad = updated_pad;
    }
    let _ = memory.save(&scratchpad).await;

    // Step 7: Evaluation (deterministic + adversarial)
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
        return Err((record, reason.into(), scratchpad));
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

    // Best-effort consolidation of learnings into long-term memory
    consolidate_learnings(
        long_term,
        &claimed.headers.correlation_id,
        &record,
        &scratchpad,
    )
    .await;

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
    long_term: &dyn MemoryStore,
    eval_llm: &dyn LlmProvider,
    eval_config: &apex_eval::EvalConfig,
    evaluator_persona: &str,
    estimator: &Arc<Mutex<TokenEstimator>>,
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
        // Refresh schemas each turn so newly created tools appear immediately
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

        // Calibrate token estimator from actual usage
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

    // Evaluation (deterministic + adversarial) for continuation
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
        return Err((record, reason.into(), scratchpad));
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

    // Best-effort consolidation of learnings into long-term memory
    consolidate_learnings(
        long_term,
        &claimed.headers.correlation_id,
        &record,
        &scratchpad,
    )
    .await;

    Ok(record)
}

// ── Consolidation ─────────────────────────────────────────────────

/// Best-effort consolidation of learnings from a successful task into long-term memory.
/// Extracts facts, skills, and strategies from the execution record.
async fn consolidate_learnings(
    store: &dyn MemoryStore,
    correlation_id: &str,
    record: &AttemptRecord,
    scratchpad: &apex_core::domain::Scratchpad,
) {
    // 1. Extract facts from "## New Facts Discovered" sections in final_text
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

    // 2. Skills: update fitness for successful tasks that used tools
    let title = &scratchpad.goal;
    if !title.is_empty() {
        match store.find_skill(title).await {
            Ok(Some(skill)) => {
                if let Err(e) = store.update_skill_fitness(&skill.id, record.outcome == AttemptOutcome::Success).await {
                    eprintln!("  consolidation: failed to update skill fitness: {e}");
                }
            }
            Ok(None) => {
                // Create a new skill record if tools were used
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

    // 3. Strategies: for jobs with subtasks, store decomposition pattern
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
                    success_count: if record.outcome == AttemptOutcome::Success { 1 } else { 0 },
                    failure_count: if record.outcome == AttemptOutcome::Failed { 1 } else { 0 },
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

// ── CLI memory commands ───────────────────────────────────────────

async fn cmd_memory_facts() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let db_path = paths.long_term_db_path();
    if !db_path.exists() {
        bail!("no long-term memory database found. Run 'apex init' first.");
    }
    let store = SqliteMemoryStore::open(&db_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let facts = store.query_facts("", 100).await?;
    if facts.is_empty() {
        println!("No facts stored.");
        return Ok(());
    }

    println!("── Facts ({}) ──", facts.len());
    println!(
        "{:<20} {:<50} {:<10} {:<20}",
        "ID", "Content", "Confidence", "Tags"
    );
    for f in &facts {
        let short_id = if f.id.0.len() > 18 {
            &f.id.0[..18]
        } else {
            &f.id.0
        };
        let content = if f.content.len() > 48 {
            format!("{}…", &f.content[..47])
        } else {
            f.content.clone()
        };
        let tags = f.tags.join(", ");
        println!(
            "{:<20} {:<50} {:<10.2} {:<20}",
            short_id, content, f.confidence, tags
        );
    }
    Ok(())
}

async fn cmd_memory_skills() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let db_path = paths.long_term_db_path();
    if !db_path.exists() {
        bail!("no long-term memory database found. Run 'apex init' first.");
    }
    let store = SqliteMemoryStore::open(&db_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Query all skills by using empty pattern
    let skill = store.find_skill("").await?;
    // For a full listing we need direct DB access; use query_facts pattern
    // Actually, let's just open DB directly for the listing
    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| anyhow::anyhow!("failed to open db: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, task_pattern, fitness, success_count, failure_count, last_used
             FROM skills ORDER BY fitness DESC",
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let rows: Vec<(String, String, f64, u32, u32, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        println!("No skills stored.");
        return Ok(());
    }

    println!("── Skills ({}) ──", rows.len());
    println!(
        "{:<20} {:<40} {:<10} {:<8} {:<8}",
        "ID", "Pattern", "Fitness", "Success", "Failure"
    );
    for (id, pattern, fitness, succ, fail, _last_used) in &rows {
        let short_id = if id.len() > 18 { &id[..18] } else { id };
        let short_pattern = if pattern.len() > 38 {
            format!("{}…", &pattern[..37])
        } else {
            pattern.clone()
        };
        println!(
            "{:<20} {:<40} {:<10.2} {:<8} {:<8}",
            short_id, short_pattern, fitness, succ, fail
        );
    }
    let _ = skill; // suppress unused warning
    Ok(())
}

async fn cmd_memory_strategies() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let db_path = paths.long_term_db_path();
    if !db_path.exists() {
        bail!("no long-term memory database found. Run 'apex init' first.");
    }

    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| anyhow::anyhow!("failed to open db: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, goal_pattern, fitness, avg_subtasks, success_count, failure_count
             FROM strategies ORDER BY fitness DESC",
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let rows: Vec<(String, String, f64, f64, u32, u32)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        println!("No strategies stored.");
        return Ok(());
    }

    println!("── Strategies ({}) ──", rows.len());
    println!(
        "{:<20} {:<40} {:<10} {:<12} {:<8} {:<8}",
        "ID", "Goal Pattern", "Fitness", "Avg Subtasks", "Success", "Failure"
    );
    for (id, pattern, fitness, avg_sub, succ, fail) in &rows {
        let short_id = if id.len() > 18 { &id[..18] } else { id };
        let short_pattern = if pattern.len() > 38 {
            format!("{}…", &pattern[..37])
        } else {
            pattern.clone()
        };
        println!(
            "{:<20} {:<40} {:<10.2} {:<12.1} {:<8} {:<8}",
            short_id, short_pattern, fitness, avg_sub, succ, fail
        );
    }
    Ok(())
}

// ── Scratch and calibration CLI commands ───────────────────────────

async fn cmd_scratch_ls() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let spill = SpillManager::new(paths.scratch_dir);
    let entries = spill.list().map_err(|e| anyhow::anyhow!("failed to list scratch: {e}"))?;

    if entries.is_empty() {
        println!("No scratch files.");
        return Ok(());
    }

    println!("── Scratch files ({}) ──", entries.len());
    for entry in &entries {
        let name = std::path::Path::new(&entry.path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let size_kb = entry.size as f64 / 1024.0;
        println!("  {name:<40} {size_kb:.1} KB");
    }
    Ok(())
}

async fn cmd_tools_list() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let manifest_path = paths.tools_dir.join("manifest.toml");

    if !manifest_path.exists() {
        println!("No custom tools. Run 'apex init' first.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&manifest_path)
        .context("failed to read tools/manifest.toml")?;

    if content.trim().is_empty() {
        println!("No custom tools registered.");
        return Ok(());
    }

    #[derive(Deserialize)]
    struct Manifest {
        #[serde(default)]
        tool: Vec<ToolEntry>,
    }
    #[derive(Deserialize)]
    struct ToolEntry {
        name: String,
        description: String,
        created_at: String,
    }

    let manifest: Manifest = toml::de::from_str(&content)
        .context("failed to parse manifest.toml")?;

    if manifest.tool.is_empty() {
        println!("No custom tools registered.");
        return Ok(());
    }

    println!("── Custom tools ({}) ──", manifest.tool.len());
    for entry in &manifest.tool {
        println!("  {:<24} {} (created: {})", entry.name, entry.description, entry.created_at);
    }
    Ok(())
}

async fn cmd_memory_calibration() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let db_path = paths.long_term_db_path();
    if !db_path.exists() {
        bail!("no long-term memory database found. Run 'apex init' first.");
    }
    let store = SqliteMemoryStore::open(&db_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let cal = store.load_calibration().await?;
    println!("── Token Calibration ──");
    println!("  Prose ratio:  {:.3} chars/token", cal.chars_per_token_prose);
    println!("  Code ratio:   {:.3} chars/token", cal.chars_per_token_code);
    println!("  Mixed ratio:  {:.3} chars/token", cal.chars_per_token_mixed);
    println!("  Sample count: {}", cal.sample_count);
    Ok(())
}

// ── Config commands ────────────────────────────────────────────────

async fn cmd_config_show() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let config = ConfigLoader::load_agent_config(&paths.config_dir)?;
    let toml_str = config.to_toml()?;
    println!("{toml_str}");
    Ok(())
}

async fn cmd_config_invariants() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let invariants = ConfigLoader::load_invariants(&paths.config_dir)?;
    let toml_str = invariants.to_toml()?;
    println!("{toml_str}");
    Ok(())
}

async fn cmd_validate() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let invariants = ConfigLoader::load_invariants(&paths.config_dir)?;
    let config = ConfigLoader::load_agent_config(&paths.config_dir)?;
    let report = validate_full(&config, &invariants, &paths.prompts_dir);

    let display = report.display();
    if report.is_ok() {
        println!("{display}");
        Ok(())
    } else {
        eprintln!("{display}");
        std::process::exit(1);
    }
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
