# SAD: **Apex** — Autonomous AI Agent Harness

**Version:** 6.0 (implementation-aligned) | **Date:** March 2026 | **Status:** Current
**Language:** Rust | **Target:** Embedded Linux (ARMv7+, AArch64, x86-64, RISC-V)

This revision reflects the actual codebase (apex-core, apex-infra, apex-eval, apex-bin) and replaces the prior design document.

---

## 1. System Purpose

Apex is a single static Rust binary that turns an embedded Linux device into an autonomous reasoning machine with persistent memory, self-improving capabilities, disciplined context management, and layered self-evaluation.

One agent. One queue. One loop. One binary. The agent accepts goals, decomposes them into tasks, executes them with tools, evaluates its own work through deterministic checks and adversarial LLM review, learns from outcomes, creates new tools when needed, and gets measurably better at your specific problems over time.

rfbmq-core is compiled directly into the binary. Every message is a rich, self-contained Markdown document carrying its own context — the task description, relevant facts, recommended approach, acceptance criteria, and execution history. The message body is the primary unit of cognitive state. `cat` on any message file in any queue directory shows you everything the agent knows, is doing, or has done.

---

## 2. System Context

```
              Human / External System
                       │
                  stdin / HTTP / cron
                       │
                ┌──────▼──────┐
                │    Apex     │  single static Rust binary
                │             │  rfbmq-core + rusqlite compiled in
                │             │  zero daemons
                └──────┬──────┘
                       │
                ┌──────▼──────┐
                │  The Agent  │  one loop, one persona
                │             │  plans + acts + evaluates + learns
                └──────┬──────┘
                       │
       ┌───────────────┼───────────────┐
       │               │               │
       ▼               ▼               ▼
    apex-infra     apex-infra      (tools run in process;
    (rfbmq queue)  (working +       no separate sandbox crate)
                   long-term +
                   token estimator)
```

rfbmq's Markdown-with-RFC-822-headers format is the native language of LLM agents. Each message is up to 64 MiB of Markdown — enough to carry full task context, execution narratives, evaluation results, and discovered facts in a single human-readable file.

---

## 3. Core Design Principles

**P1 — One agent, one loop.** A single agent loop pops a message, thinks, acts, evaluates, updates memory, and pushes results or subtasks. Behavioral mode emerges from message content.

**P2 — The message body is the cognitive unit.** Each message carries its own context: task description, relevant facts, recommended skill, acceptance criteria, and previous attempt history. A message file is a self-contained prompt. `cat` on any message in any queue directory shows the complete picture — what the agent knows, what it should do, and how to verify success.

**P3 — Memory is the architecture.** Working memory is a lightweight per-job index tracking decomposition state and failure history. Long-term memory accumulates facts, skills, and strategies across sessions. Context flows into message bodies at push-time, not pop-time — the creating agent embeds what the receiving agent needs.

**P4 — Context is token-budgeted at push-time.** When composing a subtask message, the creating agent budgets the embedded context to fit within the receiving agent's token window. A self-calibrating token estimator bridges bytes-on-disk to tokens-in-context. Large tool outputs spill to disk; the agent receives summaries and drills on demand.

**P5 — Two-layer evaluation.** Every task is evaluated through deterministic criteria first (exit codes, file existence, output matching), then adversarial LLM review for anything deterministic checks can't cover. The execution persona optimizes for success; the evaluation persona hunts for failure.

**P6 — Every function is a tool.** Decomposition, execution, evaluation, memory access, tool creation, configuration changes — all exposed with JSON schemas. Uniformly self-extensible.

**P7 — rfbmq is the coordination primitive.** One work queue backed by rfbmq-core (via apex-infra). Fan-out uses subtasks with `depends_on` headers. All coordination is atomic `rename(2)`.

**P8 — Self-extending.** The agent creates new tools, records successful approaches as skills, records effective decomposition patterns as strategies.

**P9 — Filesystem transparency.** Queue state is `ls`. Any message is `cat`. Working memory is `cat`. Long-term knowledge is `sqlite3`. The message body is the primary observability surface — rich, self-contained, human-readable.

**P10 — Declarations are mutable.** Every configuration surface except invariants is writable by the agent. The system self-configures within operator-defined guardrails.

---

## 4. Architectural Constraints

| ID | Constraint | Rationale |
|---|---|---|
| C1 | Single static binary, zero runtime deps | One-file deployment |
| C2 | Small on-device footprint | Embedded targets (actual size depends on build) |
| C3 | Linux-oriented | Primary target; cross-compilation supported |
| C4 | rfbmq-core as Rust crate dependency | Direct function calls, no process spawn |
| C5 | SQLite via rusqlite (bundled) | Single-file persistence |
| C6 | Model-agnostic | LLM provider is a trait |
| C7 | Zero-daemon constraint | Only Apex runs |
| C8 | One queue | All message types flow through `work/` |
| C9 | Max fan-out depth configurable | Prevents runaway decomposition |
| C10 | Context budgeted in tokens at push-time | Message bodies are pre-budgeted prompts |

---

## 5. Crate Structure

The workspace has four crates. Queue, memory, and LLM live in **apex-infra**; context (composer, estimator) lives in **apex-core**; tools are implemented in **apex-bin**. There are no separate apex-queue, apex-memory, apex-llm, apex-sandbox, apex-tools, or apex-context crates. Shell and file tools run in-process; there is no dedicated sandbox crate.

```
apex/
├── Cargo.toml                      # workspace root
├── crates/
│   ├── apex-core/                  # Portable — zero infra deps
│   │   ├── domain.rs               # QueueMessage, ClaimedTask, MessageHeaders, Fact, Skill, Strategy, etc.
│   │   ├── ports.rs                # Queue, LlmProvider, ToolRegistry, WorkingMemory, MemoryStore
│   │   ├── config/                  # Invariants, AgentConfig, loader, validate
│   │   ├── context/                # MessageComposer, TokenEstimator (composer.rs, estimator.rs)
│   │   └── error.rs
│   │
│   ├── apex-infra/                 # Queue + memory + LLM adapters
│   │   ├── queue/                  # RfbmqAdapter (wraps rfbmq-core)
│   │   ├── memory/                 # FsScratchpadStore (working), SqliteMemoryStore (long-term)
│   │   └── llm/                    # AnthropicProvider
│   │
│   ├── apex-eval/                  # Evaluation engine (deterministic + adversarial)
│   │   ├── evaluator.rs
│   │   ├── adversarial.rs
│   │   ├── checks.rs
│   │   └── parser.rs
│   │
│   └── apex-bin/                   # Binary entry point, CLI, agent loop, tools
│       ├── main.rs                 # CLI, path resolution, process_queue
│       ├── agent.rs                # worker_loop, execute_claim, run_agentic_loop
│       └── tools/                  # Builtin, MemoryToolRegistry, QueueToolRegistry, ConfigToolRegistry, CustomToolRegistry, spill
│
├── vendor/
│   └── rfbmq-core/                 # rfbmq-core source (path dependency)
│
└── Cargo.lock
```

### 5.1 Runtime Directory Structure

Root is `APEX_ROOT` (env var) or current directory. **Init** creates: `queues/` (and the work queue via rfbmq), `memory/working/`, `memory/long-term/`, `scratch/`, `tools/custom/`, `config/`, `tools/manifest.toml` (if missing), and writes default `invariants.toml` and `agent.toml`. Init does **not** create `prompts/` or `tools/schemas/`; those are provided by the operator or created manually.

```
{APEX_ROOT}/
├── config/
│   ├── agent.toml
│   └── invariants.toml
│
├── prompts/
│   ├── agent.md                  # execution persona (read at runtime)
│   └── evaluator.md              # adversarial evaluation persona
│
├── tools/
│   ├── manifest.toml
│   ├── schemas/                  # (optional; not created by init)
│   └── custom/
│
├── memory/
│   ├── working/                  # per-job scratchpads ({job_id}.md)
│   └── long-term/
│       └── memory.db             # SQLite: facts, skills, strategies, calibration
│
├── queues/
│   └── work/                     # rfbmq queue
│       ├── pending/
│       ├── processing/
│       ├── done/
│       └── failed/
│
└── scratch/                      # spilled tool output (ephemeral)
```

---

## 6. Message Body as Cognitive Unit

### 6.1 The Principle

Each rfbmq message is a self-contained Markdown document. The headers carry routing metadata (`message_type`, `correlation_id`, `depends_on`, `depth`, `retry_count`). The body carries cognitive state — everything the agent needs to understand and execute the task.

Context assembly happens at **push-time**, not pop-time. The agent that creates a subtask embeds the relevant facts, recommended skill, acceptance criteria, and previous attempt history into the message body. The agent that pops the subtask receives a ready-to-use prompt. This means every message file in every queue directory is a complete, inspectable document.

### 6.2 Inbound Message Format (goal / task / subtask)

```markdown
# Task: Install aws-cli

## Parent Goal
Set up nightly S3 backup of /var/data

## Context
Subtask 1 of 4 in job-42. No dependencies.
Subtasks 2 (write backup script) and 3 (test script) follow.

## Relevant Facts
- Device runs Debian 11, no Python installed
- 512MB RAM, ARM architecture
- AWS credentials at /etc/aws/credentials
- apt sources configured and reachable (verified job-38)

## Recommended Approach
Skill: package-install-apt (fitness: 0.92, 12 uses)
Use `apt-get install -y`, verify with `which`. If apt fails,
check sources list and network connectivity before retrying.

## Acceptance Criteria
### Deterministic
- command: `which aws`
  expect: exit_code 0
- command: `aws --version`
  expect: output_contains "aws-cli"
- command: `dpkg -l awscli`
  expect: exit_code 0

### Fuzzy
- Installation should not remove or conflict with existing packages

## Previous Attempts
(none — first attempt)
```

`cat queues/work/pending/task-001.md` shows everything: what to do, why, what's known, how to approach it, how to verify, and what's been tried. The agent receiving this message needs no additional context lookups to begin work.

### 6.3 Result Message Format (pushed to done/ on ack)

When a task completes, the agent rewrites the message body with the full execution narrative before acking (via `MessageComposer::compose_result`). The result includes Outcome, Execution, Final Response, Duration, Evaluation, and New Tools Created as applicable.

### 6.4 Failure Message Format (in failed/ after retry exhaustion)

`MessageComposer::compose_failure(title, attempts)` produces a failure narrative with outcome, attempt history, and (when appended by the agent) root cause and suggested resolution.

### 6.5 Continuation Message Format

The continuation message is composed when all subtasks are pushed. The agent processing the continuation reads subtask results from `done/` via `queue_read_done`, assembles the job-complete narrative with `compose_job_complete`, and consolidates learnings into long-term memory.

### 6.6 Retry: Message Body Evolution

When a task fails evaluation and is NACKed for retry, the agent rewrites the body with `MessageComposer::append_attempt` or `append_attempt_with_memory` before NACKing. The body gains a "Previous Attempts" section; the composer can compress older attempts to stay within token budget.

---

## 7. Domain Model

### 7.1 Message Headers

Headers are represented as `MessageHeaders` in apex-core (mapped to/from rfbmq custom headers):

| Field | Type | Purpose |
|-------|------|---------|
| `message_type` | Goal \| Task \| Subtask \| Continuation | Determines agent behavior mode |
| `correlation_id` | String | Links all messages in a job |
| `depth` | u32 | Decomposition depth in spawn chain |
| `retry_count` | u32 | Number of previous attempts |
| `depends_on` | Vec\<String\> | Task DAG edges (message IDs) |

Headers carry routing. The body carries cognition.

### 7.2 Working Memory

Working memory is implemented by **FsScratchpadStore** (apex-infra): one Markdown file per job at `memory/working/{job_id}.md`. The scratchpad tracks job-level overview, decomposition status, and job-level notes. Detailed execution narratives live in message bodies in `done/`. The trait `WorkingMemory` in apex-core defines `load_or_create`, `save`, `exists`, `delete`, `list_active`.

### 7.3 Long-Term Memory

Persists across jobs in SQLite at `memory/long-term/memory.db`. Implemented by **SqliteMemoryStore** (apex-infra). Tables: **facts**, **skills**, **strategies**, and calibration data. The `MemoryStore` port provides store/query/update for facts, skills, strategies, and calibration (`persist_calibration`, `load_calibration` using `CalibrationData`).

---

## 8. The Agent Loop

The agent loop lives in **apex-bin**: `main.rs` drives it via `process_queue`, which spawns one or more workers; each runs `worker_loop(ctx, worker_id)` in [crates/apex-bin/src/agent.rs](crates/apex-bin/src/agent.rs). There is no `agent_loop.rs` in apex-core.

### 8.1 Flow

1. **Pop:** `ctx.adapter.pop()` (single logical queue; no queue name parameter).
2. **Empty:** If `None`, check queue depth; if both pending and processing are 0, the worker may exit. Otherwise sleep (e.g. 1s), increment empty cycles; after a configured number of empty cycles the worker returns.
3. **Per-claim setup:** Build a `MessageComposer` from the shared `TokenEstimator`. Build `QueueToolRegistry` with queue, correlation_id, depth, max_depth, title, body, long_term, composer. Combine with static tools in `ApexToolRegistry`.
4. **Execute claim:** `execute_claim(ctx, claimed, tools)`:
   - Load or create scratchpad from working memory; optionally prepend "Working Memory" to the message body.
   - Build initial user message from body.
   - **run_agentic_loop:** Up to MAX_TURNS (e.g. 32). Each turn: call `llm.complete_with_tools`, calibrate token estimator (and periodically persist calibration to long-term memory), append assistant message and tool results to the conversation; stop when the model returns no tool calls (final text is captured).
   - **Evaluate:** `apex_eval::Evaluator::evaluate(task_body, result_text, evaluator_persona, llm, config)` — deterministic checks first, then adversarial LLM evaluation if configured.
   - On pass: build `AttemptRecord`, optionally run consolidation (extract facts, update skills/strategies), compose result body.
   - On fail or LLM error: build attempt record and failure/retry body.
5. **Success path:** `adapter.update_body(claimed, result_body)`, `adapter.ack(claimed)`.
6. **Failure path:** `handle_failure`: append attempt or compose full failure body, `update_body`, then `nack` (retry) or `ack` (terminal failure to failed/) as appropriate.

### 8.2 Context Flow

**Push-time (creating agent):** Query long-term memory for relevant facts; find best-fit skill and strategy (for goals). Compose message body with embedded context via `MessageComposer` (subtask, continuation). Budget to token limit via `TokenEstimator`. Push to work/.

**Pop-time (receiving agent):** Pop message; optionally prepend working memory to body; that (with persona) is the prompt. No additional lookups required to start.

### 8.3 Mode Emergence

**Goal →** Agent reads goal body, may query long-term memory, decomposes via `decompose_goal` tool. MessageComposer embeds context into each subtask body. Pushes subtasks and a continuation.

**Subtask →** Body is the prompt. Agent runs multi-turn LLM with tools, evaluates (deterministic then adversarial if configured). On success: compose result, ack. On failure: append attempt or compose failure, update_body, nack or ack.

**Continuation →** When dependencies are in done/, agent reads subtask results via `queue_read_done`, composes job-complete narrative, consolidates into long-term memory.

### 8.4 Self-Healing

Deterministic failure → append attempt to body with diagnosis and "Next attempt should..." guidance; nack. Adversarial finding → appended to body. Retries exhausted → compose failure narrative, ack (message moves to failed/). Missing tool → agent can call `create_tool` and retry.

---

## 9. Port Contracts

### 9.1 Queue

Single logical queue; no queue name or lease_secs in the trait. Implemented by **RfbmqAdapter** in apex-infra (wraps rfbmq-core).

```rust
#[async_trait]
pub trait Queue: Send + Sync {
    async fn push(&self, msg: QueueMessage) -> Result<String, QueueError>;
    async fn pop(&self) -> Result<Option<ClaimedTask>, QueueError>;
    async fn update_body(&self, claimed: &ClaimedTask, new_body: &str) -> Result<(), QueueError>;
    async fn ack(&self, claimed: &ClaimedTask) -> Result<(), QueueError>;
    async fn nack(&self, claimed: &ClaimedTask) -> Result<(), QueueError>;
    async fn depth(&self) -> Result<QueueDepth, QueueError>;
    async fn reap(&self) -> Result<ReapResult, QueueError>;
    async fn list_done(&self, correlation_id: &str) -> Result<Vec<String>, QueueError>;
    async fn read_done_body(&self, id: &str) -> Result<String, QueueError>;
    async fn list_with_state(&self, state: &str) -> Result<Vec<QueueMessageMeta>, QueueError>;
}
```

Types: `QueueMessage` (headers + body), `ClaimedTask` (id, claim_path, headers, body), `QueueDepth` (pending, processing), `ReapResult` (lease_reaped), `QueueMessageMeta` (id, type_label, correlation_id, depends_on).

### 9.2 LLM Provider

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, LlmError>;
    async fn complete_with_tools(&self, req: CompletionRequest, tools: &[ToolSchema])
        -> Result<ToolCompletionResponse, LlmError>;
    fn model_id(&self) -> &str;
    fn context_window(&self) -> usize;
}
```

Implemented by **AnthropicProvider** in apex-infra.

### 9.3 Working Memory

```rust
#[async_trait]
pub trait WorkingMemory: Send + Sync {
    async fn load_or_create(&self, job_id: &str) -> Result<Scratchpad, MemoryError>;
    async fn save(&self, scratchpad: &Scratchpad) -> Result<(), MemoryError>;
    async fn exists(&self, job_id: &str) -> Result<bool, MemoryError>;
    async fn delete(&self, job_id: &str) -> Result<(), MemoryError>;
    async fn list_active(&self) -> Result<Vec<String>, MemoryError>;
}
```

Implemented by **FsScratchpadStore** (files under memory/working/).

### 9.4 Long-Term Memory (MemoryStore)

```rust
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store_fact(&self, fact: Fact) -> Result<FactId, MemoryError>;
    async fn query_facts(&self, query: &str, limit: usize) -> Result<Vec<Fact>, MemoryError>;
    async fn verify_fact(&self, id: &FactId) -> Result<(), MemoryError>;

    async fn store_skill(&self, skill: Skill) -> Result<SkillId, MemoryError>;
    async fn find_skill(&self, task_pattern: &str) -> Result<Option<Skill>, MemoryError>;
    async fn list_skills(&self, limit: usize) -> Result<Vec<Skill>, MemoryError>;
    async fn update_skill_fitness(&self, id: &SkillId, success: bool) -> Result<(), MemoryError>;

    async fn store_strategy(&self, strategy: Strategy) -> Result<StrategyId, MemoryError>;
    async fn find_strategy(&self, goal: &str) -> Result<Option<Strategy>, MemoryError>;
    async fn list_strategies(&self, limit: usize) -> Result<Vec<Strategy>, MemoryError>;
    async fn update_strategy_fitness(&self, id: &StrategyId, success: bool) -> Result<(), MemoryError>;

    async fn persist_calibration(&self, data: &CalibrationData) -> Result<(), MemoryError>;
    async fn load_calibration(&self) -> Result<CalibrationData, MemoryError>;
}
```

Implemented by **SqliteMemoryStore** in apex-infra. Calibration is stored as `CalibrationData`, not a full TokenEstimator.

### 9.5 Message Composer

Lives in apex-core `context/composer.rs`. Holds a `TokenEstimator`; uses internal constants (MAX_TASK_TOKENS, MAX_FACTS_TOKENS, MAX_SKILL_TOKENS, MAX_CRITERIA_TOKENS, MAX_ATTEMPTS_TOKENS) for budgeting. No separate `ContextBudget` config struct.

```rust
pub struct MessageComposer {
    estimator: TokenEstimator,
}

impl MessageComposer {
    pub fn new(estimator: TokenEstimator) -> Self;
    pub fn compose_task_body(task: &str) -> String;
    pub fn compose_result(title: &str, record: &AttemptRecord) -> String;
    pub fn append_attempt(&self, existing_body: &str, record: &AttemptRecord) -> String;
    pub fn append_attempt_with_memory(&self, existing_body: &str, record: &AttemptRecord, scratchpad: &Scratchpad) -> String;
    pub fn compose_failure(title: &str, attempts: &[AttemptRecord]) -> String;
    pub fn compose_subtask(&self, title: &str, description: &str, acceptance_criteria: &str, parent_goal: &str, parent_context: &str) -> String;
    pub fn compose_subtask_with_memory(&self, title: &str, description: &str, acceptance_criteria: &str, parent_goal: &str, parent_context: &str, relevant_facts: &[Fact], recommended_skill: Option<&Skill>) -> String;
    pub fn compose_continuation(correlation_id: &str, goal: &str, subtask_ids: &[String]) -> String;  // static
    pub fn compose_job_complete(&self, continuation_body: &str, subtask_results: &[SubtaskResult]) -> String;
}
```

### 9.6 Evaluator

Lives in apex-eval. Static-style API; no Sandbox or TokenEstimator parameters. Persona and config are passed per call.

```rust
pub struct Evaluator;

impl Evaluator {
    pub async fn run_deterministic(body: &str) -> Option<EvalResult>;
    pub async fn evaluate(
        task_body: &str,
        result_text: &str,
        evaluator_persona: &str,
        llm: &dyn LlmProvider,
        config: &EvalConfig,
    ) -> Evaluation;
}
```

Deterministic: parse criteria from body, run checks (exit_code, output_contains, file_exists, etc.). Adversarial: run only when configured (eval_on: Always | Never | FuzzyCriteria) and when deterministic passed.

### 9.7 Sandbox

There is no Sandbox trait in apex-core ports. Built-in tools (e.g. shell_exec) run in-process. Future work may introduce a sandbox abstraction; the current implementation does not use one.

### 9.8 Tool Registry

```rust
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    fn definitions(&self) -> Vec<ToolDef>;
    fn schemas(&self) -> Vec<ToolSchema> { ... }  // default: from definitions
    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError>;
}
```

No `register`, `unregister`, or `invoke(name, input, sandbox, depth, max_depth)`. Tools are registered at build/wiring time; execution is via `execute(call)`. Implementations in apex-bin: `StaticToolRegistry` (aggregates builtin, memory, custom, config), `QueueToolRegistry` (per-claim: decompose_goal, queue_read_done), `ApexToolRegistry` (static + queue).

---

## 10. Tool System

### 10.1 Built-in Tools (as implemented)

| Registry | Tools |
|----------|--------|
| **Builtin** | `shell_exec`, `file_read`, `file_write` |
| **Memory** | `working_memory_read`, `working_memory_update`, `memory_store_fact`, `memory_query_facts`, `memory_store_skill`, `memory_query_skill`, `memory_store_strategy` |
| **Queue** (per-claim) | `decompose_goal`, `queue_read_done` |
| **Config** | `update_config` |
| **Custom** | `create_tool` plus dynamic tools loaded from `tools/manifest.toml` / `tools/custom/` |

Not present in the current implementation: `file_list`, `http_request`, `queue_push`, `queue_depth`, `queue_inspect`, `sandbox_exec`. Tool output limits and spill-to-disk are implemented in apex-bin (e.g. [crates/apex-bin/src/tools/spill.rs](crates/apex-bin/src/tools/spill.rs)); when output exceeds budget, the agent receives a summary and can drill down via `file_read` or follow-up tool calls.

### 10.2 Tool Manifest

Path: `tools/manifest.toml`. Defines custom tools and metadata; format is tool-specific. Built-in tools are wired in code (BuiltinToolRegistry, MemoryToolRegistry, etc.).

### 10.3 Dynamic Tool Creation

The agent can call `create_tool` to synthesize, test, and register a new tool. Implementations are written under `tools/custom/` and registered via the manifest. Skills can be updated when a new approach succeeds.

### 10.4 Skill Evolution

`MemoryStore::update_skill_fitness(id, success)` updates success/failure counts and fitness. Criteria templates and strategy fitness are updated during consolidation. Skills and strategies are queried when composing subtasks.

---

## 11. Context Management

### 11.1 Push-Time Budgeting

The `MessageComposer` uses a `TokenEstimator` and internal constants (MAX_TASK_TOKENS, MAX_FACTS_TOKENS, MAX_SKILL_TOKENS, MAX_CRITERIA_TOKENS, MAX_ATTEMPTS_TOKENS) to truncate sections. There is no separate `[context_budget]` section in agent.toml for per-section limits; agent.toml has `context_budget.max_body_tokens` and `max_tool_result_tokens` for high-level caps. Total context is validated against the model's context window where applicable.

### 11.2 Token Estimator

Lives in apex-core `context/estimator.rs`. Holds `CalibrationData` (chars-per-token for prose, code, mixed). Methods: `classify(text)` → ContentType, `ratio(ct)`, `estimate(text)`, `estimate_typed(text, ct)`, `budget(text, max_tokens)`, `calibrate(&mut self, prompt_text, actual_tokens)`. Calibration is persisted via `MemoryStore::persist_calibration` / `load_calibration`. Self-calibrates from LLM usage over time.

### 11.3 Tool Output Spill

When a tool's output exceeds its byte budget, the full output is written to `scratch/` and the agent receives a summary (head/tail or similar). Implemented in apex-bin (SpillManager / spill module). The agent can use `file_read` or `shell_exec` to drill into spilled files.

### 11.4 Attempt History Budget

As retries accumulate, `MessageComposer::append_attempt` and `compress_attempts` keep the "Previous Attempts" section within budget by compressing older attempts to one-line summaries.

### 11.5 Scratch File Lifecycle

Created when tool output exceeds budget; available for drill-down; cleaned up when the parent message is acked or via manual/periodic cleanup.

---

## 12. Evaluation

### 12.1 Two-Layer Stack

Layer 1: Deterministic criteria parsed from the task body (exit_code, output_contains, file_exists, etc.). Implemented in apex-eval (checks, parser). If any fail, evaluation returns passed=false and no adversarial run.

Layer 2: Adversarial LLM evaluation using `prompts/evaluator.md` persona and (optionally) a separate eval model. Run when `eval_on` is Always, or when FuzzyCriteria and the body has fuzzy criteria. Results (blocking issues, warnings) flow into attempt history for retry context.

### 12.2 Deterministic Criteria

Supported checks include exit_code, output_contains, output_matches, file_exists, file_contains, and similar. Parsed from the task body; executed by apex-eval checks module.

### 12.3 Adversarial LLM Evaluation

Persona from `prompts/evaluator.md`; optional `eval_model` in agent config. EvalConfig has `eval_on: EvalOn` (Always | Never | FuzzyCriteria).

### 12.4 Criteria Template Accumulation

Successful criteria can be stored in skills and reused; consolidation updates skill/strategy fitness and criteria templates from task outcomes.

---

## 13. Fan-Out

Independent subtasks are pushed with `depends_on` IDs. Multiple workers can pop from the same queue; dependency ordering is respected by the queue (rfbmq). Each subtask body is self-contained with embedded context.

---

## 14. Consolidation

When a continuation is processed: read subtask results from done/ via `list_done` and `read_done_body`, extract facts and update skills/strategies (when consolidation is enabled in config), compose job-complete narrative with `compose_job_complete`, update message body, ack. Working memory can be archived or cleared; scratch files purged as appropriate.

---

## 15. Configuration Layers

```
config/invariants.toml   (immutable — operator sets at deploy)
       ↓ constrains
config/agent.toml        (mutable — agent modifies via update_config tool)
       ↓ read by
worker_loop / execute_claim
```

Path resolution: `APEX_ROOT` or current directory; config dir = `{root}/config/`.

### 15.1 Invariants (invariants.toml)

Operator-defined ceilings. Loaded via `ConfigLoader::load_invariants`. Structure (apex-core):

- `limits.max_depth` (default 5)
- `limits.max_concurrent` (default 8)
- `limits.max_tools` (default 50)
- `limits.max_body_tokens` (default 100_000)
- `limits.max_retries` (default 10)

Agent config is validated against these in `validate_against_invariants`.

### 15.2 Agent Config (agent.toml)

- **agent:** model, max_concurrent, max_depth, max_retries, tools (optional list of enabled tool names; empty = all).
- **eval:** eval_model (optional override), eval_on ("always" | "never" | "fuzzy_criteria").
- **context_budget:** max_body_tokens, max_tool_result_tokens.
- **consolidation:** enabled, extract_facts, extract_skills, extract_strategies.
- **fitness:** min_pass_rate, min_uses.

Defaults and full shape are in [crates/apex-core/src/config/agent.rs](crates/apex-core/src/config/agent.rs).

---

## 16. Sandbox Model

The current implementation does not include a dedicated sandbox crate or Sandbox trait. Built-in tools such as `shell_exec` run in the process. Future versions may introduce Linux namespaces, seccomp, or cgroups for isolation; that would be documented here.

---

## 17. CLI Interface

Implemented commands:

```
apex init                           # Create runtime directory structure (queues, memory, scratch, config, tools/custom, default config files)
apex run "task description"         # Submit a goal (stdin or args), then process queue
apex work                           # Process queue (run workers until idle)
apex status                         # Show active jobs / status
apex queue                          # Show queue depth
apex queue reap                     # Reclaim expired leases
apex cat <message-path>             # Read any message body
apex tools list                     # List registered tools
apex memory facts                   # List stored facts
apex memory skills                  # List skills
apex memory strategies              # List strategies
apex memory calibration             # Show token estimator state
apex memory                         # Same as facts + skills + strategies
apex scratch ls                     # List spilled output files
apex config show                    # Show current agent config
apex config invariants              # Show operator invariants
apex validate                       # Validate config and prompts
```

There are no separate commands for `queue inspect`, `queue purge`, `tools invoke`, `memory working`, or `version` in the current implementation. `apex cat` is the primary way to inspect any message file.

---

## 18. Cross-Compilation & Deployment

```bash
cargo build --release
cross build --release --target armv7-unknown-linux-musleabihf
cross build --release --target aarch64-unknown-linux-musl
cross build --release --target riscv64gc-unknown-linux-musl
```

Deployment: copy the `apex` binary, set `APEX_ROOT` if desired, run `apex init`. Place prompts (agent.md, evaluator.md) in `prompts/` as needed.

---

## 19. Error Handling

Error types (apex-core `error.rs`):

| Error | Meaning |
|-------|---------|
| `QueueError::NotFound`, `AlreadyExists`, `Empty`, `Full`, `Io`, `Parse` | Queue operations |
| `LlmError::Http`, `Api`, `Serialization`, `UnexpectedResponse` | LLM provider |
| `ToolError::UnknownTool(name)` | Tool not in registry |
| `ToolError::InvalidInput`, `Execution` | Tool execution |
| `MemoryError::Io`, `Parse`, `NotFound`, `Database` | Memory store |

Config validation reports issues when agent config exceeds invariants (see `ValidationIssue`, `validate_against_invariants`). Evaluation failures are represented in the `Evaluation` and attempt/failure bodies rather than as separate error enums in the core.

---

## 20. Observability

| What | How |
|------|-----|
| Pending tasks | `ls queues/work/pending/` — each file is a complete task document |
| Active task | `cat queues/work/processing/*.md` |
| Completed tasks | `cat queues/work/done/*.md` |
| Failed tasks | `cat queues/work/failed/*.md` |
| Job history | grep by correlation_id in done/ then cat each |
| Working memory | `cat memory/working/{job_id}.md` |
| Long-term facts | `sqlite3 memory/long-term/memory.db "SELECT * FROM facts"` |
| Skills / strategies | Same DB, tables skills, strategies |
| Token calibration | `apex memory calibration` |
| Spilled outputs | `apex scratch ls` or `ls scratch/` |

The primary observability surface is `cat` on message files and queue directories.

---

## 21. Resource Footprint

| Component | Notes |
|-----------|--------|
| apex binary | Size depends on build (LTO, strip); target on the order of single-digit MB with rfbmq + rusqlite |
| SQLite (memory.db) | Grows with facts, skills, strategies |
| Working memory | One file per active job; lightweight markdown |
| Message files | 2–8 KB typical per message |
| Scratch | Ephemeral; purged on ack or cleanup |
| Runtime memory | Process heap for LLM client, tools, conversation buffers |

---

## 22. Open Design Questions

| # | Question | Notes |
|---|----------|--------|
| 1 | rfbmq-core vendoring | Path dependency in use; publish or subtree for releases |
| 2 | Polling vs inotify | Current: poll with sleep when empty |
| 3 | Sandbox isolation | No sandbox crate yet; shell_exec runs in-process |
| 4 | Concurrent workers | Async tasks via tokio; max_concurrent from config |
| 5 | Config hot-reload | Restart to pick up config changes |
| 6 | Done/ retention | Purge/cleanup policy left to operator or future tooling |
| 7 | Token estimator scope | Global calibration; per-model possible later |
| 8 | Adversarial eval token budget | Eval uses separate call; persona and model configurable |
