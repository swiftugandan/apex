# Apex

An autonomous AI agent harness that runs as a single static binary. Submit a goal, and Apex decomposes it into tasks, executes them with tools, learns from outcomes, and improves over time.

## Quick Start

```sh
# Build
cargo build -p apex-bin --release

# Set your Anthropic API key
export ANTHROPIC_API_KEY="sk-ant-..."

# Initialize a workspace
apex init

# Submit a goal
apex run "Refactor the parser module for clarity"

# Or pipe from stdin
echo "Write unit tests for src/auth.rs" | apex run
```

The model can be changed in `.apex/config/agent.toml` (defaults to `claude-sonnet-4-20250514`).

## How It Works

Apex is a single loop: receive a task, think, act, evaluate, learn, repeat. Every role — planning, execution, evaluation, recovery, tool creation — is the same loop with a different persona and tool set.

Tasks flow through a filesystem-based message queue. Each message is a self-contained Markdown document carrying the task description, parent context, relevant facts, recommended approach, acceptance criteria, and retry history. You can inspect any message with `cat`.

```
.apex/
├── config/          # TOML configuration (agent, invariants)
├── hooks/           # Lifecycle hooks (<event>.d/*.toml)
├── memory/
│   ├── long-term/
│   │   ├── memory.db    # SQLite fact & calibration store
│   │   └── skills/      # Learned skill files
│   └── working/         # Per-job scratchpads
├── prompts/         # Agent personas (agent.md, coder.md, ...)
├── queues/
│   ├── work/        # Main message queue (pending/, processing/, done/, failed/)
│   └── sub-<id>/    # Ephemeral per-sub-agent queues
├── scratch/         # Spilled tool outputs
└── tools/           # Custom tools (manifest + implementations)
```

## CLI Commands

| Command | Description |
|---------|-------------|
| `apex init` | Initialize `.apex/` workspace |
| `apex run "<goal>"` | Submit a goal and process it |
| `apex work` | Process pending queue messages |
| `apex status` | Show queue and system status |
| `apex queue` | Show queue depth |
| `apex queue reap` | Reclaim stale leases and clean scratch |
| `apex cat <path>` | Pretty-print a queue message |
| `apex memory` | List stored facts and skills |
| `apex memory facts` | List long-term facts |
| `apex memory skills` | List learned skills |
| `apex memory calibration` | Show token estimator calibration |
| `apex memory gc` | Garbage-collect stale working memory |
| `apex scratch ls` | List spilled scratch files |
| `apex tools list` | List registered custom tools |
| `apex config show` | Show merged agent configuration |
| `apex config invariants` | Show operator-defined invariants |
| `apex hooks list` | List registered lifecycle hooks |
| `apex hooks show <name>` | Show hook definition |
| `apex hooks validate` | Validate all hook definitions |
| `apex validate` | Validate full configuration |

## Configuration

All config lives in `.apex/config/` as TOML. Partial files work — missing fields use defaults.

**agent.toml** controls agent behavior:

```toml
[agent]
model = "claude-sonnet-4-20250514"   # LLM model
max_concurrent = 1                    # Parallel workers
max_turns = 32                        # LLM turns per task
max_depth = 5                         # Task decomposition depth
max_retries = 3                       # Retries before moving to failed/
max_output_tokens = 16384             # Max response tokens

[context_budget]
max_body_tokens = 50000               # Message context budget
max_tool_result_tokens = 10000        # Tool output before spill

[compaction]
preserve_turns = 6                    # Recent turns kept during compaction
max_summary_tokens = 1024             # Summary length when compacting

[consolidation]
enabled = true                        # Extract learnings after success
extract_facts = true
extract_skills = true
extract_strategies = true

[fitness]
min_pass_rate = 0.6                   # Min success rate to judge skill fitness
min_uses = 3                          # Min uses before judging
```

**invariants.toml** sets operator-defined hard ceilings:

```toml
[limits]
max_depth = 5              # Recursion depth
max_concurrent = 8         # Worker ceiling
max_tools = 50             # Custom tool limit
max_retries = 10           # Retry ceiling
max_sub_agent_depth = 2    # Sub-agent nesting
```

### Roles

Define sub-agent profiles in `agent.toml` for delegation:

```toml
[[roles]]
name = "coder"
persona = "coder.md"                  # Prompt file in .apex/prompts/
tools = ["shell_exec", "file_read", "file_write", "file_edit", "glob", "grep"]
can_delegate = true
memory = "shared"                     # "shared" or "isolated"

[[roles]]
name = "reviewer"
persona = "reviewer.md"
tools = ["file_read", "shell_exec"]
can_delegate = false
memory = "isolated"
```

## Hooks

Event-driven lifecycle hooks that control behavior without code changes. Each hook is a TOML file in `.apex/hooks/<event>.d/`.

**Events:** `before_turn`, `after_turn`, `before_tool_call`, `after_tool_result`, `before_push`, `after_claim`, `on_success`, `on_failure`, `on_log`

**Actions:** `script` (run shell command), `transform` (modify data), `block` (prevent event), `inject` (add context)

```toml
# .apex/hooks/on_failure.d/rate-limit-backoff.toml
[hook]
name = "rate-limit-backoff"
event = "on_failure"
priority = 10                   # Lower = runs first
invariant = true                # Operator-locked, agent can't modify
propagate = true                # Inherited by sub-agents

[action]
type = "script"
command = "python3 classify_error.py"
input = "tool_result"
timeout_ms = 5000
on_failure = "warn"             # "warn", "block", or "continue"
```

```toml
# .apex/hooks/before_turn.d/safety-reminder.toml
[hook]
name = "safety-reminder"
event = "before_turn"

[action]
type = "inject"
content = "Remember: validate all inputs before writing files."
```

```toml
# .apex/hooks/before_tool_call.d/block-rm.toml
[hook]
name = "block-rm"
event = "before_tool_call"
filter.tool = "shell_exec"     # Only fires for this tool

[action]
type = "block"
message = "Destructive shell commands are not allowed."
```

Manage hooks via CLI (`apex hooks list`, `apex hooks show <name>`, `apex hooks validate`) or at runtime through the `manage_hooks` tool.

## Task Decomposition

Complex goals can be broken into independent subtasks that run in parallel. The agent uses `decompose_goal` to push subtasks to the queue, each carrying its parent context, relevant facts, and recommended approach.

Subtasks support dependency graphs — a subtask can declare it depends on another, and Apex validates the DAG (cycle detection via topological sort) before pushing. When all subtasks complete, the parent assembles the final result from `done/` messages.

Depth is bounded by `max_depth` in config. At the limit, the agent handles tasks directly instead of decomposing further.

## Delegation

The agent can delegate tasks to sub-agents that run with their own persona, tool set, and working memory.

**Named roles** use profiles defined in `agent.toml`:

```
delegate(role="coder", task="Implement the parser module")
```

**Ad-hoc roles** define a one-off sub-agent inline:

```
delegate(
  system_prompt="You are a security auditor.",
  task="Review auth.rs for injection risks",
  tools=["file_read", "grep", "shell_exec"]
)
```

Sub-agent nesting depth is bounded by `max_sub_agent_depth` in invariants. Hooks with `propagate = true` are inherited by sub-agents.

## Memory

### Working Memory

Each job gets a scratchpad that tracks decomposition state, notes, and progress. It persists across retries — on a retry, the agent can read what the previous attempt discovered.

Tools: `working_memory_read`, `working_memory_update` (add notes, manage subtasks, update status).

### Long-Term Facts

The agent stores discovered facts (project conventions, environment details, tool versions) with confidence scores and tags. Facts are queried at push-time and embedded into subtask messages so knowledge compounds across jobs.

Tools: `memory_store_fact`, `memory_query_facts`. View with `apex memory facts`.

### Consolidation

When a task succeeds and consolidation is enabled, Apex automatically extracts facts, skills, and decomposition strategies from the execution record and stores them in long-term memory. This is how the system learns — the hundredth invocation carries knowledge from the previous ninety-nine.

## Personas

Each role loads a persona file from `.apex/prompts/` (e.g., `agent.md`, `coder.md`, `reviewer.md`). Personas define the agent's identity, turn budget, tool usage patterns, and working style. The default agent persona enforces a 32-turn budget with a gather-store-produce-verify workflow.

## Spill System

When tool output exceeds ~16KB, it's automatically spilled to `.apex/scratch/`. The agent receives a summary envelope with the first and last 20 lines, total size, and pattern counts (errors, warnings, failures). The full output is preserved on disk for targeted reads with `file_read` using offset/limit.

## Custom Tools

Agents can create shell-script tools at runtime. Tools receive JSON on stdin, return JSON on stdout.

```
.apex/tools/
├── manifest.toml
└── custom/
    └── csv-parser/
        ├── run.sh          # Implementation (receives JSON stdin)
        ├── schema.json     # Input JSON schema
        └── test.sh         # Must exit 0 to register
```

The agent creates tools via the `create_tool` built-in — it writes the script, schema, and test, runs the test, and registers the tool in `manifest.toml`. Tools are available immediately after creation.

**manifest.toml:**

```toml
[[tool]]
name = "csv-parser"
description = "Parse CSV files into JSON"
script = "run.sh"
schema_file = "schema.json"
timeout_secs = 30
task_pattern = "parse.*csv"       # Optional — auto-creates a matching skill
created_at = "2026-03-11T10:30:00Z"
```

If `task_pattern` is set, the tool auto-registers a skill so future tasks matching that pattern discover and use the tool.

## Skills

Skills capture successful approaches that compound over time. Each skill records a task pattern, the approach that worked, which tools were used, acceptance criteria, and a fitness score (success/failure ratio).

When the agent decomposes a goal or delegates a task, it queries the skill store for matching patterns. If a skill matches, its proven approach, tools, and criteria are embedded directly into the subtask message — the sub-agent receives a ready-to-use strategy instead of starting from scratch.

The agent can also query skills explicitly with `memory_query_skill` to find the best approach before attempting a task, and store new skills with `memory_store_skill`. When consolidation is enabled, skills are automatically extracted after successful task completion. Low-fitness skills are retired over time.

You can also create skills by hand. Skills are markdown files with YAML frontmatter in `.apex/memory/long-term/skills/`:

```markdown
---
name: deploy-rust-service
description: "Deploy a Rust microservice to production"
id: skill-deploy-rust
task_pattern: "deploy.*rust.*service"
tools_used: [shell_exec, file_read, file_write]
success_count: 0
failure_count: 0
fitness: 0.50
min_samples: 3
last_used: "2026-03-11T00:00:00Z"
status: active
---

## Approach

1. Run cargo build --release
2. Copy binary to /opt/services/
3. Restart the systemd unit
4. Verify health endpoint returns 200

## Acceptance Criteria

- Binary compiles without errors
- Service responds on health endpoint within 10s

## Notes

Requires SSH access to the deploy target.
```

View stored skills with `apex memory skills`.

## Architecture

See [CLAUDE.md](CLAUDE.md) for crate structure, dependency graph, and architectural details.

## Building

```sh
cargo build -p apex-bin --release
```

Produces a single optimized binary (LTO, stripped, panic=abort).

## References

| Resource | Description |
|----------|-------------|
| [The Apex Manifesto](docs/MANIFESTO.md) | Design principles behind the system |
| [CLAUDE.md](CLAUDE.md) | Crate structure, dependency graph, architectural details |
| [Apex Wiki](https://deepwiki.com/swiftugandan/apex) | Browsable project documentation |
| [rfbmq Wiki](https://deepwiki.com/swiftugandan/rfbmq) | Documentation for the filesystem-based message queue |

## License

[MIT](LICENSE)
