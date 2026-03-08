use std::io::Read;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use apex_core::domain::{ChatMessage, CompletionRequest, ContentBlock, MessageRole, ToolResult};
use apex_core::ports::{LlmProvider, ToolRegistry};
use apex_llm::anthropic::AnthropicProvider;
use apex_tools::BuiltinToolRegistry;

const MAX_TURNS: usize = 32;
const MAX_TOKENS: u32 = 8192;

fn find_prompts_dir() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        let candidate = dir.join("prompts");
        if candidate.is_dir() {
            return Ok(candidate);
        }
        if !dir.pop() {
            bail!("could not find prompts/ directory");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
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
                bail!("no task provided. Usage: apex run \"<task>\" or echo \"<task>\" | apex run");
            }

            run_agent(task).await
        }
        Some(cmd) => bail!("unknown command: {cmd}. Available: run"),
        None => bail!("no command provided. Usage: apex run \"<task>\""),
    }
}

async fn run_agent(task: String) -> Result<()> {
    let prompts_dir = find_prompts_dir()?;
    let persona_path = prompts_dir.join("agent.md");
    let persona =
        std::fs::read_to_string(&persona_path).context("failed to read prompts/agent.md")?;

    let llm = AnthropicProvider::from_env();
    let tools = BuiltinToolRegistry::new();
    let schemas = tools.schemas();

    let mut messages = vec![ChatMessage::user_text(&task)];

    eprintln!("▶ Task: {task}");

    for turn in 0..MAX_TURNS {
        let req = CompletionRequest {
            system_prompt: persona.clone(),
            messages: messages.clone(),
            max_tokens: MAX_TOKENS,
            temperature: Some(0.2),
        };

        let resp = llm.complete_with_tools(req, &schemas).await?;

        eprintln!(
            "  turn {}: {} tool call(s), {} input / {} output tokens",
            turn + 1,
            resp.tool_calls.len(),
            resp.usage.input_tokens,
            resp.usage.output_tokens,
        );

        // Append the assistant's message to the transcript
        messages.push(resp.message.clone());

        // If no tool calls, this is the final response
        if resp.tool_calls.is_empty() {
            let text = resp.text();
            if !text.is_empty() {
                println!("{text}");
            }
            return Ok(());
        }

        // Execute tool calls and collect results
        let mut result_blocks = Vec::new();
        for call in &resp.tool_calls {
            eprintln!("  ↳ {}(…)", call.name);
            let result = match tools.execute(call).await {
                Ok(r) => r,
                Err(err) => ToolResult {
                    tool_use_id: call.id.clone(),
                    name: call.name.clone(),
                    output: serde_json::json!({ "error": err.to_string() }),
                    is_error: true,
                },
            };

            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: result.tool_use_id,
                content: serde_json::to_string(&result.output).unwrap_or_else(|_| "{}".to_string()),
                is_error: result.is_error,
            });
        }

        // Append tool results as a user message
        messages.push(ChatMessage {
            role: MessageRole::User,
            content: result_blocks,
        });
    }

    bail!("agent exceeded maximum turns ({MAX_TURNS})");
}
