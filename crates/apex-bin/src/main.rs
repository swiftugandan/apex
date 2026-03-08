use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};

use apex_context::MessageComposer;
use apex_core::domain::{
    AttemptOutcome, AttemptRecord, ChatMessage, ClaimedTask, CompletionRequest, ContentBlock,
    MessageHeaders, MessageRole, MessageType, QueueMessage, ToolCallRecord, ToolResult, TurnRecord,
};
use apex_core::ports::{LlmProvider, Queue, ToolRegistry};
use apex_llm::anthropic::AnthropicProvider;
use apex_queue::RfbmqAdapter;
use apex_tools::BuiltinToolRegistry;

const MAX_TURNS: usize = 32;
const MAX_TOKENS: u32 = 8192;
const MAX_RETRIES: u32 = 3;

// ── Path resolution ────────────────────────────────────────────────

struct ApexPaths {
    root: PathBuf,
    prompts_dir: PathBuf,
    queue_dir: PathBuf,
}

impl ApexPaths {
    fn resolve() -> Result<Self> {
        let root = std::env::var("APEX_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        Ok(Self {
            prompts_dir: root.join("prompts"),
            queue_dir: root.join("queues").join("work"),
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
        Some(cmd) => bail!("unknown command: {cmd}. Available: init, run, queue, cat, work"),
        None => bail!("no command provided. Usage: apex <command>"),
    }
}

// ── Commands ───────────────────────────────────────────────────────

async fn cmd_init() -> Result<()> {
    let paths = ApexPaths::resolve()?;

    std::fs::create_dir_all(&paths.queue_dir.parent().unwrap())
        .context("failed to create queues/ directory")?;

    RfbmqAdapter::init(&paths.queue_dir).map_err(|e| anyhow::anyhow!("{e}"))?;

    eprintln!("✓ Initialized apex at {}", paths.root.display());
    eprintln!("  queue: {}", paths.queue_dir.display());
    Ok(())
}

async fn cmd_run(task: String) -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let adapter = open_queue(&paths)?;

    let correlation_id = format!("job-{}", &uuid_v4()[..8]);
    let body = MessageComposer::compose_task_body(&task);

    let msg = QueueMessage {
        headers: MessageHeaders {
            message_type: MessageType::Task,
            correlation_id: correlation_id.clone(),
            depth: 0,
            retry_count: 0,
        },
        body,
    };

    let id = adapter.push(msg).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    eprintln!("▶ Queued task {id} (correlation: {correlation_id})");

    // Process the queue until our task is done
    process_queue(&paths, &adapter).await
}

async fn cmd_work() -> Result<()> {
    let paths = ApexPaths::resolve()?;
    let adapter = open_queue(&paths)?;
    process_queue(&paths, &adapter).await
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

async fn process_queue(paths: &ApexPaths, adapter: &RfbmqAdapter) -> Result<()> {
    let persona_path = paths.prompts_dir.join("agent.md");
    let persona =
        std::fs::read_to_string(&persona_path).context("failed to read prompts/agent.md")?;

    let llm = AnthropicProvider::from_env();
    let tools = BuiltinToolRegistry::new();

    loop {
        let claimed = adapter
            .pop()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let claimed = match claimed {
            Some(c) => c,
            None => {
                // Queue is empty — if we came from `apex run`, we're done
                return Ok(());
            }
        };

        eprintln!(
            "▶ Processing task {} (retry {})",
            claimed.id, claimed.headers.retry_count
        );

        match execute_task(&claimed, &persona, &llm, &tools).await {
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
                eprintln!("✓ Task {} completed successfully", claimed.id);
            }
            Err((record, err)) => {
                eprintln!("✗ Task {} failed: {err}", claimed.id);

                let updated_body =
                    MessageComposer::append_attempt(&claimed.body, &record);

                adapter
                    .update_body(&claimed, &updated_body)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;

                adapter
                    .nack(&claimed)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;

                if claimed.headers.retry_count + 1 >= MAX_RETRIES {
                    eprintln!("  ↳ Max retries reached, message moved to failed/");
                } else {
                    eprintln!(
                        "  ↳ Requeued for retry (attempt {} of {})",
                        claimed.headers.retry_count + 2,
                        MAX_RETRIES
                    );
                }
            }
        }

        // Check if there are more tasks
        let depth = adapter
            .depth()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if depth.pending + depth.processing == 0 {
            return Ok(());
        }
    }
}

// ── Agent loop (multi-turn LLM + tool execution) ───────────────────

async fn execute_task(
    claimed: &ClaimedTask,
    persona: &str,
    llm: &AnthropicProvider,
    tools: &BuiltinToolRegistry,
) -> std::result::Result<AttemptRecord, (AttemptRecord, String)> {
    let started_at = now_iso();
    let schemas = tools.schemas();

    let mut messages = vec![ChatMessage::user_text(&claimed.body)];
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
                };
                return Err((record, format!("LLM error: {err}")));
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
            // Record the final turn (no tool calls)
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

    let record = AttemptRecord {
        attempt_number: claimed.headers.retry_count + 1,
        started_at,
        finished_at: now_iso(),
        turns,
        final_text,
        outcome: AttemptOutcome::Success,
        failure_reason: None,
    };

    Ok(record)
}

// ── Utilities ──────────────────────────────────────────────────────

fn extract_title(body: &str) -> String {
    for line in body.lines() {
        if let Some(title) = line.strip_prefix("# Task: ") {
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
