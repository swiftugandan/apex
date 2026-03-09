use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use apex_core::config::{ConfigLoader, validate_full};
use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::domain::{MessageHeaders, MessageType, QueueMessage};
use apex_core::ports::{MemoryStore, Queue, SkillStore, WorkingMemory};
use apex_engine::{
    InProcessSpawner, InfraFactories, ProjectPaths, SpawnerConfig, WorkerContext,
    build_static_tools, worker_loop,
};
use apex_infra::{AnthropicProvider, FsSkillStore, FsScratchpadStore, RfbmqAdapter, SqliteMemoryStore};
use apex_tools::spill::SpillManager;

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
                Some("calibration") => cmd_memory_calibration().await,
                None => {
                    cmd_memory_facts().await?;
                    cmd_memory_skills().await
                }
                Some(sub) => bail!("unknown memory subcommand: {sub}. Available: facts, skills, calibration"),
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
    let paths = ProjectPaths::resolve()?;
    paths.create_dirs()?;

    let manifest_path = paths.tools_dir.join("manifest.toml");
    if !manifest_path.exists() {
        std::fs::write(&manifest_path, "")
            .context("failed to write tools/manifest.toml")?;
    }

    ConfigLoader::write_default_invariants(&paths.config_dir)?;
    ConfigLoader::write_default_agent_config(&paths.config_dir)?;

    write_default_prompts(&paths.prompts_dir)?;

    RfbmqAdapter::init(&paths.work_queue).map_err(|e| anyhow::anyhow!("{e}"))?;

    eprintln!("✓ Initialized apex at {}", paths.root.display());
    eprintln!("  queue:    {}", paths.work_queue.display());
    eprintln!("  memory:   {}", paths.working_memory.display());
    eprintln!("  long-term: {}", paths.long_term_dir.display());
    eprintln!("  scratch:  {}", paths.scratch_dir.display());
    eprintln!("  tools:    {}", paths.tools_dir.display());
    eprintln!("  config:   {}", paths.config_dir.display());
    eprintln!("  prompts:  {}", paths.prompts_dir.display());
    Ok(())
}

async fn cmd_run(task: String) -> Result<()> {
    let paths = ProjectPaths::resolve()?;
    let adapter = open_queue(&paths)?;

    // Drain stale pending messages from previous runs
    let drained = drain_pending(&adapter).await?;
    if drained > 0 {
        eprintln!("⚠ Drained {drained} stale pending message(s) from previous runs");
    }

    let correlation_id = apex_core::generate_id("job");
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

/// Pop and reject all stale pending messages, moving them to failed/.
async fn drain_pending(adapter: &RfbmqAdapter) -> Result<u32> {
    let mut count = 0u32;
    loop {
        let claimed = adapter
            .pop()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        match claimed {
            Some(task) => {
                adapter
                    .reject(&task)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                count += 1;
            }
            None => break,
        }
    }
    Ok(count)
}

async fn cmd_work() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
    let adapter = open_queue(&paths)?;
    process_queue(&paths, Arc::new(adapter)).await
}

async fn cmd_queue_depth() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
    let adapter = open_queue(&paths)?;

    let d = adapter
        .depth()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("{}", d.pending + d.processing);
    Ok(())
}

async fn cmd_queue_reap() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
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
    let paths = ProjectPaths::resolve()?;
    let adapter = open_queue(&paths)?;

    let dirs = ["pending", "processing", "done", "failed"];
    for dir_name in &dirs {
        let list = adapter
            .list_with_state(dir_name)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if list.is_empty() {
            continue;
        }

        println!("── {dir_name}/ ({} messages) ──", list.len());
        for meta in &list {
            let deps = if meta.depends_on.is_empty() {
                String::new()
            } else {
                format!(" depends_on=[{}]", meta.depends_on.join(", "))
            };
            let short_id = &meta.id[..meta.id.len().min(12)];
            println!(
                "  {short_id}  {:<13} corr={}{deps}",
                meta.type_label, meta.correlation_id
            );
        }
    }

    Ok(())
}

// ── Default prompts (embedded from prompts/ at compile time) ─────

const DEFAULT_AGENT_PROMPT: &str = include_str!("../../../prompts/agent.md");
const DEFAULT_CODER_PROMPT: &str = include_str!("../../../prompts/coder.md");
const DEFAULT_REVIEWER_PROMPT: &str = include_str!("../../../prompts/reviewer.md");

fn write_default_prompts(prompts_dir: &Path) -> Result<()> {
    for (filename, content) in [
        ("agent.md", DEFAULT_AGENT_PROMPT),
        ("coder.md", DEFAULT_CODER_PROMPT),
        ("reviewer.md", DEFAULT_REVIEWER_PROMPT),
    ] {
        let path = prompts_dir.join(filename);
        if !path.exists() {
            std::fs::write(&path, content)
                .with_context(|| format!("failed to write prompts/{filename}"))?;
        }
    }
    Ok(())
}

// ── Queue helpers ──────────────────────────────────────────────────

fn open_queue(paths: &ProjectPaths) -> Result<RfbmqAdapter> {
    RfbmqAdapter::open(&paths.work_queue).map_err(|e| {
        anyhow::anyhow!(
            "failed to open queue at {}. Run 'apex init' first. Error: {e}",
            paths.work_queue.display()
        )
    })
}

// ── Queue processing loop ──────────────────────────────────────────

async fn process_queue(paths: &ProjectPaths, adapter: Arc<RfbmqAdapter>) -> Result<()> {
    let persona_path = paths.prompts_dir.join("agent.md");
    let persona =
        std::fs::read_to_string(&persona_path).context("failed to read prompts/agent.md")?;

    let invariants = ConfigLoader::load_invariants(&paths.config_dir)?;
    let agent_config = ConfigLoader::load_agent_config(&paths.config_dir)?;

    let max_concurrent = agent_config.agent.max_concurrent;
    let max_depth = agent_config.agent.max_depth;
    let max_retries = agent_config.agent.max_retries;
    let max_tool_result_bytes = agent_config.context_budget.max_tool_result_tokens * 4;
    let remaining_delegate_depth = invariants.limits.max_sub_agent_depth;
    let roles: Arc<[apex_core::config::RoleProfile]> = agent_config.roles.clone().into();

    let llm: Arc<dyn apex_core::ports::LlmProvider> = Arc::new(
        AnthropicProvider::from_env_with_model(&agent_config.agent.model)
            .context("failed to create LLM provider")?
    );

    let memory: Arc<dyn WorkingMemory> =
        Arc::new(FsScratchpadStore::new(paths.working_memory.clone()));

    let long_term: Arc<dyn MemoryStore> = Arc::new(
        SqliteMemoryStore::open(&paths.long_term_db())
            .context("failed to open long-term memory database")?,
    );

    let skills: Arc<dyn SkillStore> = Arc::new(FsSkillStore::new(paths.skills_dir.clone()));

    let calibration = long_term.load_calibration().await.unwrap_or_default();
    let estimator = Arc::new(Mutex::new(TokenEstimator::new(calibration)));

    let invariants = Arc::new(invariants);

    // Build the SubAgentSpawner with factory closures for infra creation
    let infra = Arc::new(InfraFactories {
        queue: Arc::new(|path| {
            RfbmqAdapter::init(path)
                .map(|a| Arc::new(a) as Arc<dyn Queue>)
                .map_err(|e| e.to_string())
        }),
        working_memory: Arc::new(|path| {
            Arc::new(FsScratchpadStore::new(path.to_path_buf())) as Arc<dyn WorkingMemory>
        }),
        memory_store: Arc::new(|path| {
            SqliteMemoryStore::open(path)
                .map(|s| Arc::new(s) as Arc<dyn MemoryStore>)
                .map_err(|e| e.to_string())
        }),
        skill_store: Arc::new(|path| {
            Arc::new(FsSkillStore::new(path.to_path_buf())) as Arc<dyn SkillStore>
        }),
    });
    let spawner: Arc<dyn apex_tools::SubAgentSpawner> = Arc::new(InProcessSpawner {
        project_paths: paths.clone(),
        parent_long_term: long_term.clone(),
        parent_skills: skills.clone(),
        llm: llm.clone(),
        estimator: estimator.clone(),
        config: SpawnerConfig {
            invariants: Arc::clone(&invariants),
            roles: Arc::clone(&roles),
            max_tool_result_bytes,
            remaining_delegate_depth,
        },
        infra,
    });

    let static_tools = build_static_tools(
        paths,
        memory.clone(),
        long_term.clone(),
        skills.clone(),
        Arc::clone(&invariants),
        spawner,
        roles,
        remaining_delegate_depth,
    );

    let queue: Arc<dyn Queue> = adapter.clone();

    let ctx = WorkerContext {
        queue,
        tools: static_tools,
        llm,
        memory,
        long_term,
        skills,
        persona: Arc::new(persona),
        max_depth,
        max_retries,
        max_tool_result_bytes,
        estimator,
    };

    if max_concurrent <= 1 {
        worker_loop(ctx, 0).await
    } else {
        let mut handles = Vec::new();
        for worker_id in 0..max_concurrent {
            let worker_ctx = ctx.clone();
            handles.push(tokio::spawn(async move {
                worker_loop(worker_ctx, worker_id as usize).await
            }));
        }

        for handle in handles {
            handle.await??;
        }
        Ok(())
    }
}

// ── CLI memory commands ───────────────────────────────────────────

fn short_id(id: &str, max: usize) -> &str {
    if id.len() > max { &id[..max] } else { id }
}

fn truncate_col(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", apex_core::truncate_str(s, max - 1))
    } else {
        s.to_string()
    }
}

/// Open the long-term memory store and run the given closure.
async fn with_memory_store<F, Fut>(paths: &ProjectPaths, f: F) -> Result<()>
where
    F: FnOnce(SqliteMemoryStore) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let db_path = paths.long_term_db();
    if !db_path.exists() {
        bail!("no long-term memory database found. Run 'apex init' first.");
    }
    let store = SqliteMemoryStore::open(&db_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    f(store).await
}

async fn cmd_memory_facts() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
    with_memory_store(&paths, |store| async move {
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
            let short_id = short_id(&f.id.0, 18);
            let content = truncate_col(&f.content, 48);
            let tags = f.tags.join(", ");
            println!(
                "{:<20} {:<50} {:<10.2} {:<20}",
                short_id, content, f.confidence, tags
            );
        }
        Ok(())
    })
    .await
}

async fn cmd_memory_skills() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
    let skill_store = FsSkillStore::new(paths.skills_dir.clone());
    let skills = skill_store
        .list_skills(100)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if skills.is_empty() {
        println!("No skills stored.");
        return Ok(());
    }
    println!("── Skills ({}) ──", skills.len());
    println!(
        "{:<20} {:<24} {:<30} {:<10} {:<8} {:<8} {:<8}",
        "ID", "Name", "Description", "Fitness", "Success", "Failure", "Status"
    );
    for s in &skills {
        let short_id = short_id(&s.id.0, 18);
        let short_name = truncate_col(&s.name, 22);
        let short_desc = truncate_col(&s.description, 28);
        println!(
            "{:<20} {:<24} {:<30} {:<10.2} {:<8} {:<8} {:<8}",
            short_id, short_name, short_desc, s.fitness, s.success_count, s.failure_count, s.status
        );
    }
    Ok(())
}

// ── Scratch and calibration CLI commands ───────────────────────────

async fn cmd_scratch_ls() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
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
    let paths = ProjectPaths::resolve()?;
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
    let paths = ProjectPaths::resolve()?;
    with_memory_store(&paths, |store| async move {
        use apex_core::ports::MemoryStore;
        let cal = store.load_calibration().await?;
        println!("── Token Calibration ──");
        println!("  Prose ratio:  {:.3} chars/token", cal.chars_per_token_prose);
        println!("  Code ratio:   {:.3} chars/token", cal.chars_per_token_code);
        println!("  Mixed ratio:  {:.3} chars/token", cal.chars_per_token_mixed);
        println!("  Sample count: {}", cal.sample_count);
        Ok(())
    })
    .await
}

// ── Config commands ────────────────────────────────────────────────

async fn cmd_config_show() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
    let config = ConfigLoader::load_agent_config(&paths.config_dir)?;
    let toml_str = config.to_toml()?;
    println!("{toml_str}");
    Ok(())
}

async fn cmd_config_invariants() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
    let invariants = ConfigLoader::load_invariants(&paths.config_dir)?;
    let toml_str = invariants.to_toml()?;
    println!("{toml_str}");
    Ok(())
}

async fn cmd_validate() -> Result<()> {
    let paths = ProjectPaths::resolve()?;
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

