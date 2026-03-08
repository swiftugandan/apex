mod agent;
mod tools;

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use apex_core::config::{ConfigLoader, validate_full};
use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::domain::{MessageHeaders, MessageType, QueueMessage};
use apex_core::ports::{LlmProvider, Queue, WorkingMemory};
use apex_infra::{AnthropicProvider, FsScratchpadStore, RfbmqAdapter, SqliteMemoryStore};

use crate::agent::WorkerContext;
use crate::tools::spill::SpillManager;

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

    let manifest_path = paths.tools_dir.join("manifest.toml");
    if !manifest_path.exists() {
        std::fs::write(&manifest_path, "")
            .context("failed to write tools/manifest.toml")?;
    }

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

    let eval_llm: Arc<dyn LlmProvider> = if let Some(ref eval_model) = eval_config.eval_model {
        let api_key =
            std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set");
        Arc::new(AnthropicProvider::new(api_key, eval_model.clone(), 200_000))
    } else {
        Arc::clone(&llm) as Arc<dyn LlmProvider>
    };

    let memory: Arc<dyn WorkingMemory> =
        Arc::new(FsScratchpadStore::new(paths.memory_dir.clone()));

    let long_term: Arc<dyn apex_core::ports::MemoryStore> = Arc::new(
        SqliteMemoryStore::open(&paths.long_term_db_path())
            .context("failed to open long-term memory database")?,
    );

    let calibration = long_term.load_calibration().await.unwrap_or_default();
    let estimator = Arc::new(Mutex::new(TokenEstimator::new(calibration)));

    let invariants = Arc::new(invariants);
    let static_tools = agent::build_static_tools(
        paths.scratch_dir.clone(),
        paths.tools_dir.clone(),
        paths.config_dir.clone(),
        memory.clone(),
        long_term.clone(),
        Arc::clone(&invariants),
    );

    let ctx = WorkerContext {
        adapter: adapter.clone(),
        static_tools,
        llm,
        eval_llm,
        memory,
        long_term,
        persona: Arc::new(persona),
        evaluator_persona: Arc::new(evaluator_persona),
        eval_config: Arc::new(eval_config),
        max_depth,
        max_retries,
        scratch_dir: paths.scratch_dir.clone(),
        tools_dir: paths.tools_dir.clone(),
        config_dir: paths.config_dir.clone(),
        invariants,
        estimator,
    };

    if max_concurrent <= 1 {
        agent::worker_loop(ctx, 0).await
    } else {
        // For multiple workers, we need to clone the context fields
        let mut handles = Vec::new();
        for worker_id in 0..max_concurrent {
            let ctx = WorkerContext {
                adapter: Arc::clone(&ctx.adapter),
                static_tools: Arc::clone(&ctx.static_tools),
                llm: Arc::clone(&ctx.llm),
                eval_llm: Arc::clone(&ctx.eval_llm),
                memory: Arc::clone(&ctx.memory),
                long_term: Arc::clone(&ctx.long_term),
                persona: Arc::clone(&ctx.persona),
                evaluator_persona: Arc::clone(&ctx.evaluator_persona),
                eval_config: Arc::clone(&ctx.eval_config),
                max_depth: ctx.max_depth,
                max_retries: ctx.max_retries,
                scratch_dir: ctx.scratch_dir.clone(),
                tools_dir: ctx.tools_dir.clone(),
                config_dir: ctx.config_dir.clone(),
                invariants: Arc::clone(&ctx.invariants),
                estimator: Arc::clone(&ctx.estimator),
            };

            handles.push(tokio::spawn(async move {
                agent::worker_loop(ctx, worker_id as usize).await
            }));
        }

        for handle in handles {
            handle.await??;
        }
        Ok(())
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

    use apex_core::ports::MemoryStore;
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

    use apex_core::ports::MemoryStore;
    let skills = store.list_skills(100).await?;
    if skills.is_empty() {
        println!("No skills stored.");
        return Ok(());
    }

    println!("── Skills ({}) ──", skills.len());
    println!(
        "{:<20} {:<40} {:<10} {:<8} {:<8}",
        "ID", "Pattern", "Fitness", "Success", "Failure"
    );
    for s in &skills {
        let short_id = if s.id.0.len() > 18 { &s.id.0[..18] } else { &s.id.0 };
        let short_pattern = if s.task_pattern.len() > 38 {
            format!("{}…", &s.task_pattern[..37])
        } else {
            s.task_pattern.clone()
        };
        println!(
            "{:<20} {:<40} {:<10.2} {:<8} {:<8}",
            short_id, short_pattern, s.fitness, s.success_count, s.failure_count
        );
    }
    Ok(())
}

async fn cmd_memory_strategies() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let db_path = paths.long_term_db_path();
    if !db_path.exists() {
        bail!("no long-term memory database found. Run 'apex init' first.");
    }
    let store = SqliteMemoryStore::open(&db_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    use apex_core::ports::MemoryStore;
    let strategies = store.list_strategies(100).await?;
    if strategies.is_empty() {
        println!("No strategies stored.");
        return Ok(());
    }

    println!("── Strategies ({}) ──", strategies.len());
    println!(
        "{:<20} {:<40} {:<10} {:<12} {:<8} {:<8}",
        "ID", "Goal Pattern", "Fitness", "Avg Subtasks", "Success", "Failure"
    );
    for s in &strategies {
        let short_id = if s.id.0.len() > 18 { &s.id.0[..18] } else { &s.id.0 };
        let short_pattern = if s.goal_pattern.len() > 38 {
            format!("{}…", &s.goal_pattern[..37])
        } else {
            s.goal_pattern.clone()
        };
        println!(
            "{:<20} {:<40} {:<10.2} {:<12.1} {:<8} {:<8}",
            short_id, short_pattern, s.fitness, s.avg_subtasks, s.success_count, s.failure_count
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

    use apex_core::ports::MemoryStore;
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

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{:016x}{:08x}", t, pid)
}
