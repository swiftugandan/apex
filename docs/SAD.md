# SAD: **Apex** — Autonomous AI Agent Harness

**Version:** 5.0 | **Date:** March 2026 | **Status:** Draft
**Language:** Rust | **Target:** Embedded Linux (ARMv7+, AArch64, x86-64, RISC-V)

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
                │             │  < 6 MB, zero daemons
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
    rfbmq-core      memory          sandbox
    (linked,        (working +      (Linux namespaces)
     dirs on disk)  long-term +
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

**P6 — Every function is a tool.** Decomposition, execution, evaluation, memory access, tool creation, configuration changes — all registered with JSON schemas. Uniformly self-extensible.

**P7 — rfbmq is the coordination primitive.** One work queue backed by rfbmq-core. Fan-out uses subtasks with `Depends-On` headers. `Queue::ready()` resolves dependency ordering. All coordination is atomic `rename(2)`.

**P8 — Self-extending.** The agent creates new tools, records successful approaches as skills, records effective decomposition patterns as strategies.

**P9 — Filesystem transparency.** Queue state is `ls`. Any message is `cat`. Working memory is `cat`. Long-term knowledge is `sqlite3`. The message body is the primary observability surface — rich, self-contained, human-readable.

**P10 — Declarations are mutable.** Every configuration surface except invariants is writable by the agent. The system self-configures within operator-defined guardrails.

---

## 4. Architectural Constraints

| ID | Constraint | Rationale |
|---|---|---|
| C1 | Single static binary, zero runtime deps | One-file deployment |
| C2 | < 6 MB on-device footprint | Embedded targets |
| C3 | Linux-only (kernel 4.18+) | Namespace/seccomp/cgroup sandboxing |
| C4 | rfbmq-core as Rust crate dependency | Direct function calls, no process spawn |
| C5 | SQLite via rusqlite (bundled) | Single-file persistence |
| C6 | Model-agnostic | LLM provider is a trait |
| C7 | Zero-daemon constraint | Only Apex runs |
| C8 | One queue | All message types flow through `work/` |
| C9 | Max fan-out depth: 2 (configurable) | Prevents runaway decomposition |
| C10 | Context budgeted in tokens at push-time | Message bodies are pre-budgeted prompts |

---

## 5. Crate Structure

```
apex/
├── Cargo.toml                      # workspace root
├── crates/
│   ├── apex-core/                  # PORTABLE — zero infra deps
│   │   ├── domain/                 # Job, Task, QueueMessage, Memory, Tool, Skill
│   │   ├── ports/                  # trait contracts (§9)
│   │   └── use_cases/
│   │       └── agent_loop.rs       # the agent loop
│   │
│   ├── apex-queue/                 # Queue port adapter — wraps rfbmq-core
│   │   └── Cargo.toml              # depends on rfbmq-core
│   │
│   ├── apex-memory/                # SQLite: working memory + long-term + calibration
│   ├── apex-llm/                   # LLM provider adapters
│   ├── apex-sandbox/               # Linux namespace sandbox
│   ├── apex-tools/                 # Built-in tool implementations
│   ├── apex-context/               # Token estimator + message body composer
│   ├── apex-eval/                  # Evaluation engine (deterministic + adversarial)
│   └── apex-bin/                   # Binary entry point, DI wiring, CLI, config
│
├── vendor/
│   └── rfbmq-core/                 # rfbmq-core source
│
└── Cargo.lock
```

### 5.1 Runtime Directory Structure

```
/opt/apex/
├── config/
│   ├── agent.toml
│   ├── invariants.toml
│   └── models.toml
│
├── prompts/
│   ├── agent.md                  # execution persona
│   └── evaluator.md              # adversarial evaluation persona
│
├── tools/
│   ├── manifest.toml
│   ├── schemas/
│   └── custom/
│
├── memory/
│   ├── working/                  # lightweight per-job scratchpads
│   └── long-term/
│       └── apex.db               # SQLite: facts + skills + strategies + calibration
│
├── queues/
│   └── work/                     # rfbmq queue
│       ├── pending/              # ← self-contained task documents
│       ├── processing/           # ← task being worked
│       ├── done/                 # ← complete execution narratives
│       └── failed/               # ← failure narratives with full history
│
├── scratch/                      # spilled tool output (ephemeral)
│
└── sandbox/                      # tool execution workspace
```

---

## 6. Message Body as Cognitive Unit

### 6.1 The Principle

Each rfbmq message is a self-contained Markdown document. The headers carry routing metadata (`Type`, `Correlation-Id`, `Depends-On`, `Depth`, `Priority`). The body carries cognitive state — everything the agent needs to understand and execute the task.

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

When a task completes, the agent rewrites the message body with the full execution narrative before acking:

```markdown
# Result: Install aws-cli

## Outcome
SUCCESS

## Execution
1. Ran `apt-get update` — exit 0, 12s
2. Ran `apt-get install -y awscli` — exit 0, 34s
3. Verified `which aws` → `/usr/bin/aws`
4. Verified `aws --version` → `aws-cli/1.22.34`

## Evaluation
### Deterministic: PASS (3/3)
- `which aws` → exit 0 ✓
- `aws --version` → contains "aws-cli" ✓
- `dpkg -l awscli` → exit 0 ✓

### Adversarial: PASS
No blocking issues. Warning: awscli from apt may be outdated
compared to pip install. Acceptable for backup use case.

## New Facts Discovered
- awscli version 1.22.34 installed via apt
- apt-get update takes ~12s on this network

## Duration
48s total
```

`cat queues/work/done/task-001.md` is a complete audit record. No cross-referencing needed.

### 6.4 Failure Message Format (in failed/ after retry exhaustion)

```markdown
# Failed: Configure SSL certificate

## Outcome
FAILED (3/3 retries exhausted)

## Attempt History
### Attempt 1
- Ran `certbot --nginx -d example.com`
- Exit code 1: "Could not bind to port 80"
- Diagnosis: nginx already listening on port 80
- Deterministic eval: FAIL — exit_code != 0

### Attempt 2
- Stopped nginx, ran certbot, restarted nginx
- Exit code 1: "DNS problem: NXDOMAIN"
- Diagnosis: domain not pointed to this device's IP
- Deterministic eval: FAIL — exit_code != 0

### Attempt 3
- Attempted DNS verification method
- Exit code 1: "No DNS plugin installed"
- Deterministic eval: FAIL — exit_code != 0

## Root Cause Assessment
Domain example.com does not resolve to this device.
SSL provisioning requires DNS configuration outside
this device's control.

## Suggested Resolution
Configure DNS A record for example.com to point to
this device's public IP before retrying.
```

`ls queues/work/failed/` and `cat` on any file gives the operator a complete failure narrative with all attempts, diagnoses, and resolution suggestions. No log diving required.

### 6.5 Continuation Message Format

The continuation message fires when all subtasks complete. The creating agent doesn't know the results yet — it only knows the structure. The agent processing the continuation reads subtask results from `done/` and assembles them into the final output:

```markdown
# Continuation: job-42

## Goal
Set up nightly S3 backup of /var/data

## Subtask IDs
- task-001: Install aws-cli
- task-002: Write backup script
- task-003: Test backup script
- task-004: Create cron entry

## Instructions
All subtasks above have completed. Read their results from done/,
assemble a summary, extract facts and skills for consolidation,
and produce the final job result.
```

After processing, the continuation's result body in `done/` becomes the complete job narrative:

```markdown
# Job Complete: job-42

## Goal
Set up nightly S3 backup of /var/data

## Result
Nightly backup configured and verified.

## Subtask Summary
### 1. Install aws-cli — SUCCESS (48s)
awscli 1.22.34 installed via apt.

### 2. Write backup script — SUCCESS (22s)
/opt/backup.sh created. Uses aws s3 sync with flock
and error handling. Shellcheck passed.

### 3. Test backup script — SUCCESS (67s)
Dry-run: 47 files, 198MB total. No errors.

### 4. Create cron entry — SUCCESS (8s)
/etc/cron.d/backup-var-data, runs 02:00 daily.

## Facts Discovered
- awscli 1.22.34 installed via apt
- /opt/backup.sh exists, uses aws s3 sync
- /var/data is 198MB, 47 files
- Backup cron at 02:00 daily via /etc/cron.d/

## Skills Updated
- package-install-apt: success (fitness → 0.93)
- s3-backup-awscli: success (fitness → 0.88)
- cron-setup-etc-cron-d: success (fitness → 1.0)

## Total Duration
145s
```

### 6.6 Retry: Message Body Evolution

When a task fails evaluation and is NACKed for retry, rfbmq puts it back in `pending/` with an incremented retry count. The agent rewrites the body before NACKing to include the attempt history:

```markdown
# Task: Install aws-cli

## Parent Goal
Set up nightly S3 backup of /var/data

## Relevant Facts
- Device runs Debian 11, no Python installed
- 512MB RAM, ARM architecture

## Recommended Approach
Skill: package-install-apt (fitness: 0.92)

## Acceptance Criteria
### Deterministic
- command: `which aws`
  expect: exit_code 0

### Fuzzy
- Should not conflict with existing packages

## Previous Attempts
### Attempt 1 — FAILED
- Ran `apt-get install -y awscli`
- Exit code 100: "Unable to locate package awscli"
- Root cause: apt sources not updated
- Deterministic eval: FAIL — `which aws` exit 1
- Adversarial eval: skipped (deterministic failed)

**→ Next attempt should run `apt-get update` first.**
```

The retry agent sees exactly what was tried and what went wrong. The "Next attempt should" line is written by the agent based on its diagnosis — it's giving its future self specific instructions.

---

## 7. Domain Model

### 7.1 Message Headers

```
Id: a3f2e1b4c5d6a7b8
Type: goal | task | subtask | continuation
Correlation-Id: job-42
Depends-On: task-001, task-002
Depth: 0
Priority: normal
TTL: 3600
Retry-Count: 0
```

| Header | Purpose |
|---|---|
| `Id` | Unique message identifier |
| `Type` | Determines agent behavior mode |
| `Correlation-Id` | Links all messages in a job |
| `Depends-On` | Task DAG edges (comma-separated IDs) |
| `Depth` | Decomposition depth in spawn chain |
| `Priority` | critical / high / normal / low |
| `TTL` | Time-to-live in seconds |
| `Retry-Count` | Number of previous attempts |

Headers carry routing. The body carries cognition.

### 7.2 Working Memory

Working memory is lighter in this design. The rich execution context lives in message bodies. The scratchpad at `memory/working/{job-id}.md` tracks only the job-level overview:

```markdown
# Working Memory: job-42

## Goal
Set up nightly S3 backup of /var/data

## Decomposition
1. [done] Install aws-cli → task-001
2. [done] Write backup script → task-002
3. [active] Test backup script → task-003
4. [pending] Create cron entry → task-004 (depends on 003)

## Status
3 of 4 subtasks complete. Waiting on task-003.

## Job-Level Notes
- Switched from boto3 to aws-cli approach early (no Python on device)
- apt sources needed update before install
```

Detailed execution narratives, evaluation results, and discovered facts live in the message bodies in `done/`. The scratchpad is an index, not a comprehensive log. This keeps working memory small and well within the token budget.

### 7.3 Long-Term Memory

Persists across jobs in SQLite (`memory/long-term/apex.db`).

**Facts** — environment knowledge.

```sql
CREATE TABLE facts (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    source_job TEXT,
    confidence REAL DEFAULT 1.0,
    created_at TEXT,
    last_verified TEXT,
    tags TEXT
);
```

**Skills** — effective approaches for task types.

```sql
CREATE TABLE skills (
    id TEXT PRIMARY KEY,
    task_pattern TEXT NOT NULL,
    approach TEXT NOT NULL,
    tools_used TEXT,
    criteria_template TEXT,
    success_count INTEGER DEFAULT 0,
    failure_count INTEGER DEFAULT 0,
    fitness REAL DEFAULT 0.0,
    min_samples INTEGER DEFAULT 5,
    last_used TEXT,
    parent_id TEXT,
    notes TEXT
);
```

**Strategies** — decomposition patterns for goal types.

```sql
CREATE TABLE strategies (
    id TEXT PRIMARY KEY,
    goal_pattern TEXT NOT NULL,
    decomposition TEXT NOT NULL,
    avg_subtasks REAL,
    avg_duration_secs REAL,
    success_count INTEGER DEFAULT 0,
    failure_count INTEGER DEFAULT 0,
    fitness REAL DEFAULT 0.0,
    notes TEXT
);
```

**Calibration** — token estimator state.

```sql
CREATE TABLE calibration (
    id TEXT PRIMARY KEY DEFAULT 'default',
    chars_per_token_prose REAL DEFAULT 4.0,
    chars_per_token_code REAL DEFAULT 3.0,
    chars_per_token_mixed REAL DEFAULT 3.5,
    sample_count INTEGER DEFAULT 0,
    updated_at TEXT
);
```

---

## 8. The Agent Loop

```rust
pub async fn agent_loop(
    config: &AgentConfig,
    queue: &dyn Queue,
    llm: &dyn LlmProvider,
    memory: &dyn MemoryStore,
    working_mem: &dyn WorkingMemory,
    tools: &dyn ToolRegistry,
    sandbox: &dyn Sandbox,
    composer: &MessageComposer,
    evaluator: &Evaluator,
    estimator: &mut TokenEstimator,
) -> Result<(), AgentError> {
    let persona = load_persona(&config.persona)?;

    loop {
        // 1. Pop next ready message
        let claimed = match queue.pop("work", config.lease_secs).await? {
            Some(msg) => msg,
            None => { sleep(config.poll_interval).await; continue; }
        };

        let msg_type = claimed.headers.msg_type;
        let job_id = &claimed.headers.correlation_id;
        let depth = claimed.headers.depth.unwrap_or(0);

        // 2. Load or create lightweight working memory
        let mut scratch = working_mem.load_or_create(job_id).await?;

        // 3. The message body IS the context.
        //    Persona is the only thing added at pop-time.
        let prompt = format!("{}\n\n{}", persona, claimed.body);

        // 4. Call LLM
        let tool_schemas = tools.resolve_schemas(&config.tools).await?;
        let response = llm.complete_with_tools(
            CompletionRequest {
                system: prompt,
                messages: vec![],
                model: config.model.clone(),
            },
            &tool_schemas,
        ).await?;

        // 5. Calibrate token estimator
        estimator.calibrate_from_response(&response.usage);
        memory.persist_calibration(estimator).await?;

        // 6. Execute tool calls
        let mut results = Vec::new();
        for tool_call in &response.tool_calls {
            let result = tools.invoke(
                &tool_call.name, tool_call.input.clone(),
                sandbox, depth, config.max_depth,
            ).await?;
            results.push(result);
        }

        // 7. Two-layer evaluation
        let evaluation = evaluator.evaluate(
            &claimed.body,
            &format_result(&response, &results),
            sandbox, llm, &config.eval, estimator,
        ).await?;

        // 8. Handle failure — rewrite body with attempt history, nack
        if !evaluation.passed {
            let retry_count = claimed.headers.retry_count.unwrap_or(0);
            if retry_count < config.max_retries {
                let updated_body = composer.append_attempt(
                    &claimed.body, retry_count + 1,
                    &response, &results, &evaluation,
                );
                queue.update_body("work", &claimed.id, &updated_body).await?;
                scratch.record_failure_summary(&evaluation);
                working_mem.save(&scratch).await?;
                queue.nack("work", &claimed.id, true).await?;
                continue;
            }
            // Terminal failure — rewrite body with full failure narrative
            let failure_body = composer.compose_failure(
                &claimed.body, &response, &results, &evaluation,
            );
            queue.update_body("work", &claimed.id, &failure_body).await?;
        }

        // 9. Success — rewrite body with execution narrative
        if evaluation.passed {
            let result_body = composer.compose_result(
                &claimed.body, &response, &results, &evaluation,
            );
            queue.update_body("work", &claimed.id, &result_body).await?;
        }

        // 10. Update working memory (lightweight)
        scratch.update_subtask_status(&claimed, &evaluation);
        working_mem.save(&scratch).await?;

        // 11. Handle subtask/continuation creation for goals
        match msg_type {
            MessageType::Goal => {
                // Agent decomposed via tool calls.
                // Each subtask pushed by decompose_goal tool
                // with full context embedded by the MessageComposer.
                if has_pending_subtasks(&scratch) {
                    let continuation_body = composer.compose_continuation(
                        job_id, &scratch,
                    );
                    queue.push("work", QueueMessage {
                        msg_type: MessageType::Continuation,
                        correlation_id: job_id.clone(),
                        depends_on: collect_subtask_ids(&scratch),
                        depth,
                        body: continuation_body,
                        ..Default::default()
                    }).await?;
                }
            }
            MessageType::Continuation => {
                // Read subtask results from done/, consolidate
                let subtask_results = read_subtask_results(
                    queue, job_id
                ).await?;
                let job_body = composer.compose_job_complete(
                    &claimed.body, &subtask_results,
                );
                queue.update_body("work", &claimed.id, &job_body).await?;
                consolidate(job_id, &subtask_results, memory).await?;
                cleanup_scratch(job_id).await?;
            }
            _ => {}
        }

        // 12. Ack
        queue.ack("work", &claimed.id).await?;
    }
}
```

### 8.1 Context Flow

```
Push-time (creating agent):
  Query long-term memory for relevant facts
  Find best-fit skill and criteria template
  Find best-fit strategy (for goals)
  Compose message body with embedded context
  Budget to token limit via TokenEstimator
  Push to work/

Pop-time (receiving agent):
  Pop message
  Prepend persona to message body
  That's the prompt. No additional lookups needed.
```

The creating agent does the heavy lifting — it has just been reasoning about the goal and knows exactly which context is relevant for each subtask. The receiving agent gets a pre-assembled, pre-budgeted prompt. This eliminates redundant memory queries and ensures each subtask gets precisely the context it needs, not a generic dump of recent facts.

### 8.2 Mode Emergence

**Goal arrives →** Agent reads the goal body (which may include user-provided context). Queries long-term memory for matching strategy and relevant facts. Decomposes into subtasks via `decompose_goal` tool. For each subtask, `MessageComposer` embeds relevant facts, best-fit skill, criteria template, and parent context into the body. Pushes subtasks to `work/` with `Depends-On` edges. Pushes a `continuation`.

**Subtask arrives →** Body is a complete prompt. Agent prepends persona, calls LLM, executes tools, runs two-layer evaluation. On success, rewrites body as execution narrative, acks. On failure, appends attempt history to body, nacks for retry.

**Continuation arrives →** `Queue::ready()` surfaces this when all dependencies are in `done/`. Agent reads subtask results from `done/`, assembles job-complete narrative, consolidates learnings into long-term memory.

### 8.3 Self-Healing

1. Deterministic criterion fails → agent composes diagnosis, appends to body as "Previous Attempts" section, includes "Next attempt should..." guidance.
2. Adversarial eval finds blocking issue → specific finding appended to body.
3. `nack` returns message to `pending/` with incremented retry count and enriched body.
4. Next pop: the body contains the full failure history. The agent reads it as part of the prompt and addresses the specific issues.
5. Missing tool → agent calls `create_tool`, retries.
6. Retries exhausted → body rewritten as failure narrative with all attempts and root cause assessment. Message moves to `failed/`.

---

## 9. Port Contracts

### 9.1 Queue

```rust
#[async_trait]
pub trait Queue: Send + Sync {
    async fn init(&self, queue: &str, max_pending: Option<u32>) -> Result<(), QueueError>;
    async fn push(&self, queue: &str, msg: QueueMessage) -> Result<String, QueueError>;
    async fn pop(&self, queue: &str, lease_secs: u32) -> Result<Option<ClaimedMessage>, QueueError>;
    async fn ack(&self, queue: &str, msg_id: &str) -> Result<(), QueueError>;
    async fn nack(&self, queue: &str, msg_id: &str, retry: bool) -> Result<(), QueueError>;
    async fn update_body(&self, queue: &str, msg_id: &str, body: &str) -> Result<(), QueueError>;
    async fn depth(&self, queue: &str) -> Result<QueueDepth, QueueError>;
    async fn ready(&self, queue: &str) -> Result<Vec<String>, QueueError>;
    async fn inspect(&self, msg_path: &str) -> Result<MessageMetadata, QueueError>;
    async fn cat(&self, msg_path: &str) -> Result<String, QueueError>;
    async fn reap(&self, queue: &str) -> Result<ReapResult, QueueError>;
}
```

`update_body` is the new operation — it rewrites the message body while preserving headers. Used before both ack (to write the result narrative) and nack (to append attempt history). The rfbmq-core adapter implements this as an atomic write-to-temp-then-rename, preserving rfbmq's durability guarantees.

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

Working memory is lighter now — the `Scratchpad` tracks decomposition status and job-level notes. Detailed execution narratives live in message bodies. No `load_compressed` or `load_section` needed; the scratchpad fits in the token budget naturally because it's an index, not a comprehensive log.

### 9.4 Long-Term Memory

```rust
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store_fact(&self, fact: Fact) -> Result<FactId, MemoryError>;
    async fn query_facts(&self, query: &str, limit: usize) -> Result<Vec<Fact>, MemoryError>;
    async fn verify_fact(&self, id: &FactId) -> Result<(), MemoryError>;

    async fn store_skill(&self, skill: Skill) -> Result<SkillId, MemoryError>;
    async fn find_skill(&self, task_pattern: &str) -> Result<Option<Skill>, MemoryError>;
    async fn update_skill_fitness(&self, id: &SkillId, outcome: &Outcome)
        -> Result<(), MemoryError>;

    async fn store_strategy(&self, strategy: Strategy) -> Result<StrategyId, MemoryError>;
    async fn find_strategy(&self, goal: &str) -> Result<Option<Strategy>, MemoryError>;
    async fn update_strategy_fitness(&self, id: &StrategyId, outcome: &Outcome)
        -> Result<(), MemoryError>;

    async fn persist_calibration(&self, estimator: &TokenEstimator) -> Result<(), MemoryError>;
    async fn load_calibration(&self) -> Result<Option<TokenEstimator>, MemoryError>;
}
```

### 9.5 Message Composer

```rust
pub struct MessageComposer {
    estimator: TokenEstimator,
    budget: ContextBudget,
}

impl MessageComposer {
    /// Compose a subtask message body with embedded context
    pub fn compose_subtask(
        &self,
        task_description: &str,
        parent_goal: &str,
        facts: &[Fact],
        skill: Option<&Skill>,
        depth_context: &str,
    ) -> String;

    /// Compose result narrative after successful execution
    pub fn compose_result(
        &self,
        original_body: &str,
        response: &ToolCompletionResponse,
        results: &[ToolResult],
        evaluation: &Evaluation,
    ) -> String;

    /// Append attempt history to body for retry
    pub fn append_attempt(
        &self,
        original_body: &str,
        attempt_num: u32,
        response: &ToolCompletionResponse,
        results: &[ToolResult],
        evaluation: &Evaluation,
    ) -> String;

    /// Compose failure narrative after retry exhaustion
    pub fn compose_failure(
        &self,
        original_body: &str,
        response: &ToolCompletionResponse,
        results: &[ToolResult],
        evaluation: &Evaluation,
    ) -> String;

    /// Compose continuation message
    pub fn compose_continuation(
        &self,
        job_id: &str,
        scratch: &Scratchpad,
    ) -> String;

    /// Compose job-complete narrative from subtask results
    pub fn compose_job_complete(
        &self,
        continuation_body: &str,
        subtask_results: &[SubtaskResult],
    ) -> String;
}
```

The `MessageComposer` is the component that enforces token budgets at push-time. When composing a subtask body, it allocates tokens across sections (context, facts, skill, criteria) and truncates to fit. The resulting message body is pre-budgeted — the receiving agent's pop-time context assembly is just "prepend persona."

### 9.6 Evaluator

```rust
pub struct Evaluator {
    eval_persona: String,
}

impl Evaluator {
    pub async fn evaluate(
        &self,
        task_body: &str,
        result: &str,
        sandbox: &dyn Sandbox,
        llm: &dyn LlmProvider,
        config: &EvalConfig,
        estimator: &TokenEstimator,
    ) -> Result<Evaluation, EvalError> {
        // Layer 1: deterministic
        let deterministic = self.run_deterministic(task_body, sandbox).await?;
        if !deterministic.passed {
            return Ok(Evaluation {
                deterministic,
                adversarial: None,
                passed: false,
                blocking_issues: deterministic.failures.clone(),
                warnings: vec![],
            });
        }

        // Layer 2: adversarial
        let fuzzy_criteria = extract_fuzzy_criteria(task_body);
        let needs_adversarial = match config.eval_on {
            EvalOn::Always => true,
            EvalOn::FuzzyCriteria => !fuzzy_criteria.is_empty(),
            EvalOn::Never => false,
        };

        let adversarial = if needs_adversarial {
            Some(self.run_adversarial(
                task_body, result, &fuzzy_criteria,
                llm, config, estimator
            ).await?)
        } else {
            None
        };

        let passed = adversarial.as_ref().map(|a| a.passed).unwrap_or(true);

        Ok(Evaluation {
            deterministic,
            adversarial,
            passed,
            blocking_issues: adversarial.as_ref()
                .map(|a| a.blocking_issues.clone()).unwrap_or_default(),
            warnings: adversarial.as_ref()
                .map(|a| a.warnings.clone()).unwrap_or_default(),
        })
    }
}
```

### 9.7 Sandbox

```rust
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn execute(&self, cmd: SandboxCommand) -> Result<SandboxResult, SandboxError>;
    async fn execute_with_timeout(&self, cmd: SandboxCommand, timeout: Duration)
        -> Result<SandboxResult, SandboxError>;
    fn capabilities(&self) -> SandboxCapabilities;
}
```

### 9.8 Tool Registry

```rust
#[async_trait]
pub trait ToolRegistry: Send + Sync {
    async fn register(&self, tool: ToolDef) -> Result<(), ToolError>;
    async fn unregister(&self, name: &str) -> Result<(), ToolError>;
    async fn get(&self, name: &str) -> Result<Option<ToolDef>, ToolError>;
    async fn list(&self) -> Result<Vec<ToolDef>, ToolError>;
    async fn list_by_tag(&self, tag: &str) -> Result<Vec<ToolDef>, ToolError>;
    async fn resolve_schemas(&self, names: &[String]) -> Result<Vec<ToolSchema>, ToolError>;
    async fn invoke(&self, name: &str, input: serde_json::Value,
                    sandbox: &dyn Sandbox, depth: u8, max_depth: u8)
        -> Result<ToolResult, ToolError>;
}
```

---

## 10. Tool System

### 10.1 Built-in Tools

| Tool | Description | Default Output Budget |
|---|---|---|
| `shell_exec` | Execute a shell command via sandbox | 8KB |
| `file_read` | Read a file (supports line ranges) | 8KB |
| `file_write` | Write content to a file | — |
| `file_list` | List directory contents | 4KB |
| `http_request` | Make an HTTP request | 8KB |
| `memory_store_fact` | Store a fact in long-term memory | — |
| `memory_query_facts` | Query facts by relevance | 4KB |
| `memory_store_skill` | Store or update a skill | — |
| `memory_query_skill` | Find best skill for a task type | 2KB |
| `memory_store_strategy` | Store or update a strategy | — |
| `working_memory_read` | Read scratchpad | 4KB |
| `working_memory_update` | Update scratchpad | — |
| `queue_push` | Push a message to the work queue | — |
| `queue_depth` | Check queue depth | 256B |
| `queue_inspect` | Inspect a message's headers | 2KB |
| `queue_read_done` | Read a completed message from done/ | 8KB |
| `create_tool` | Synthesize, test, and register a new tool | 4KB |
| `decompose_goal` | Break a goal into subtasks with embedded context | 4KB |
| `sandbox_exec` | Execute in isolated namespace | 8KB |
| `update_config` | Modify agent.toml (validated against invariants) | — |

`decompose_goal` now calls `MessageComposer` internally to embed context into each subtask's body before pushing to the queue. `queue_read_done` allows the continuation agent to read subtask results from `done/`.

### 10.2 Tool Manifest

```toml
[[tool]]
name = "shell_exec"
description = "Execute a shell command in the sandbox"
mode = "builtin"
schema = "schemas/shell_exec.json"
sandbox = true
network = false
max_output = "8KB"
spill_strategy = "head_tail"
head_lines = 20
tail_lines = 20
```

### 10.3 Dynamic Tool Creation

1. LLM generates implementation.
2. Written to `tools/custom/{name}/`.
3. Tested in sandbox.
4. Entry added to `tools/manifest.toml`.
5. Skill record created.
6. Tool immediately available.

### 10.4 Skill Evolution

Each use updates fitness. `criteria_template` accumulates proven acceptance criteria. Skills below `auto_retire_below` are excluded. `parent_id` tracks lineage.

---

## 11. Context Management

### 11.1 Push-Time Budgeting

Context is budgeted when composing message bodies, not when popping them. The `MessageComposer` allocates tokens across sections:

```toml
[context_budget]
max_body_tokens = 6000          # total budget for message body
max_task_tokens = 1000          # task description + parent context
max_facts_tokens = 1000         # relevant facts section
max_skill_tokens = 500          # recommended approach
max_criteria_tokens = 500       # acceptance criteria
max_attempts_tokens = 2000      # previous attempt history (grows with retries)
```

The persona (prepended at pop-time) has its own budget:

```toml
max_persona_tokens = 500
```

Total tokens entering the LLM: `max_persona_tokens + max_body_tokens + tool_schemas_overhead`. This is validated against the model's context window via `LlmProvider::context_window()`.

### 11.2 Token Estimator

Self-calibrating, persists in SQLite:

```rust
pub struct TokenEstimator {
    chars_per_token_prose: f32,   // initial: 4.0
    chars_per_token_code: f32,    // initial: 3.0
    chars_per_token_mixed: f32,   // initial: 3.5
    sample_count: u32,
}
```

Converges to ~5% accuracy after 20-30 LLM calls. Calibrated from `response.usage.prompt_tokens` after every call.

### 11.3 Tool Output Spill

When output exceeds a tool's `max_output`, the full output writes to `scratch/`. The agent receives a summary envelope:

```markdown
## Tool Result: shell_exec
Status: exit_code 0
Output: SPILLED (247KB, 3,841 lines → /scratch/result-a7b3.txt)

### Head (first 20 lines)
[first 20 lines]

### Tail (last 20 lines)
[last 20 lines]

### Stats
- Total lines: 3,841
- Total bytes: 247,129
- Patterns: error (12), warning (47), info (3,782)
```

The agent drills with follow-up tool calls. Tools support pre-filtering (`grep`, `tail`, `max_lines`). The agent learns to use filtered invocations via skills.

### 11.4 Attempt History Budget

As retries accumulate, the "Previous Attempts" section grows. The `MessageComposer` manages this within the `max_attempts_tokens` budget. If three attempts exceed the budget, the oldest attempt is compressed to a one-line summary:

```markdown
## Previous Attempts
### Attempt 1 [COMPRESSED]
✗ apt-get install failed — package not found (sources not updated)

### Attempt 2 [FULL]
- Ran `apt-get update` — exit 0
- Ran `apt-get install -y awscli` — exit 100: "held broken packages"
- Diagnosis: conflicting dependency with existing aws-sdk package
- Deterministic eval: FAIL

**→ Next attempt should resolve the dependency conflict first.**
```

### 11.5 Scratch File Lifecycle

- Created when tool output exceeds byte budget.
- Available for drill-down via `file_read` and `shell_exec`.
- Deleted when parent message is ACKed.
- Cleaned up by `apex reap` if orphaned.

---

## 12. Evaluation

### 12.1 Two-Layer Stack

```
Task result
     │
     ▼
Layer 1: Deterministic criteria
     │   exit codes, file existence, output matching
     │
     │   fail? → append to body, nack
     │   pass? ──────────────────────────┐
     │                                    │
     ▼                                    ▼
Layer 2: Adversarial LLM evaluation     (if fuzzy criteria
     │   different persona,              or eval_on = "always")
     │   optionally different model
     │
     │   fail? → append findings to body, nack
     │   pass? → compose result narrative, ack
```

### 12.2 Deterministic Criteria

| Check | Description |
|---|---|
| `exit_code` | Command exits with expected code |
| `output_contains` | stdout contains a string |
| `output_matches` | stdout matches a regex |
| `file_exists` | File exists at path |
| `file_contains` | File contains a string |
| `file_size` | File size within range |
| `http_status` | HTTP endpoint returns expected status |
| `json_path` | JSON field matches expected value |
| `not_contains` | Output does not contain a string |

### 12.3 Adversarial LLM Evaluation

Different persona (`prompts/evaluator.md`), optionally different model (`eval_model`). Finds blocking issues and warnings. Specific findings flow into the message body's attempt history for retry context.

```toml
[eval]
eval_model = "claude-sonnet-4-20250514"
eval_on = "fuzzy_criteria"
```

### 12.4 Criteria Template Accumulation

Successful criteria are stored in the skill's `criteria_template`. Reused on future instances of the same task pattern. Criteria that catch real failures are promoted. Evaluation quality compounds with use.

---

## 13. Fan-Out

Independent subtasks execute in parallel. Each subtask's message body carries its own embedded context:

```
Goal: "Set up monitoring with Prometheus and Grafana"

Subtasks pushed to work/:
  task-A body: full context for installing Prometheus
  task-B body: full context for installing Grafana
  task-C body: full context for Prometheus config (Depends-On: A)
  task-D body: full context for Grafana dashboard (Depends-On: B, C)
  task-E body: full context for e2e test (Depends-On: D)
  continuation: instructions for final assembly (Depends-On: E)
```

Multiple agent loop instances pop from the same queue. `Queue::ready()` ensures ordering. Each subtask is self-contained — no shared state between parallel tasks.

---

## 14. Consolidation

When a `continuation` fires:

1. Read subtask results from `done/` — each is a complete narrative with "New Facts Discovered" and execution details.
2. Extract facts → `store_fact`.
3. Update skill fitness → `update_skill_fitness`.
4. Update skill `criteria_template` with criteria that caught real failures.
5. Update strategy fitness → `update_strategy_fitness`.
6. Record new skills if novel approaches succeeded.
7. Compose job-complete narrative → write to message body.
8. Archive or delete working memory.
9. Purge `scratch/` files.

Consolidation reads primarily from message bodies in `done/`, not from scattered logs or databases. The `## New Facts Discovered` section in each result body tells the consolidation step exactly what to extract.

---

## 15. Configuration Layers

```
invariants.toml    (immutable — operator sets at deploy)
       ↓ constrains
agent.toml         (mutable — agent modifies via update_config)
       ↓ read by
agent_loop         (runtime)
```

### 15.1 Invariants

```toml
[limits]
max_depth = 5
max_concurrent = 8
max_tools = 200
max_pending = 5000
max_memory_per_sandbox = "512M"
max_cpu_time_per_tool = "60s"
max_working_memory_size = "1M"
max_fact_count = 10000
max_body_tokens = 12000
max_tool_output = "64KB"
```

### 15.2 Agent Config

```toml
[agent]
persona = "prompts/agent.md"
model = "claude-sonnet-4-20250514"
tools = ["shell_exec", "file_read", "file_write", "file_list",
         "http_request", "memory_store_fact", "memory_query_facts",
         "memory_store_skill", "memory_query_skill",
         "memory_store_strategy", "working_memory_read",
         "working_memory_update", "queue_push", "queue_depth",
         "queue_read_done", "create_tool", "decompose_goal",
         "sandbox_exec", "update_config"]
max_concurrent = 4
max_depth = 2
lease_secs = 120
max_retries = 3
poll_interval_ms = 1000

[eval]
eval_model = "claude-sonnet-4-20250514"
eval_on = "fuzzy_criteria"

[context_budget]
max_body_tokens = 6000
max_task_tokens = 1000
max_facts_tokens = 1000
max_skill_tokens = 500
max_criteria_tokens = 500
max_attempts_tokens = 2000
max_persona_tokens = 500

[consolidation]
auto = true
extract_facts = true
update_skills = true
update_strategies = true
archive_working_memory = true

[fitness]
min_success_rate = 0.70
min_samples = 5
auto_retire_below = 0.30
```

---

## 16. Sandbox Model

- **Mount namespace**: read-only root, writable tmpfs for workspace
- **PID namespace**: isolated process tree
- **Network namespace**: disabled by default, opt-in per tool
- **Seccomp filter**: syscall whitelist
- **Cgroups**: memory and CPU limits from config/invariants
- **UID mapping**: unprivileged user inside namespace

`NoopSandbox` for devices with limited namespace support.

---

## 17. CLI Interface

```
apex init                                     # Create runtime directory structure
apex run "set up a backup cron job"           # Submit a goal
apex status                                    # Show active jobs
apex status job-42                            # Show working memory
apex queue                                     # Show queue depth
apex queue inspect                             # List pending messages
apex queue reap                                # Reclaim expired leases
apex queue purge                               # Clean old done/ messages
apex cat <message-path>                        # Read any message body
apex tools list                                # List registered tools
apex tools invoke shell_exec '{"cmd":"ls"}'    # Direct tool invocation
apex memory facts                              # List stored facts
apex memory skills                             # List skills with fitness
apex memory strategies                         # List strategies with fitness
apex memory working                            # List active scratchpads
apex memory calibration                        # Show token estimator state
apex config show                               # Show current agent config
apex config invariants                         # Show operator invariants
apex validate                                  # Validate all declarations
apex scratch ls                                # List spilled output files
apex version                                   # Print version
```

`apex cat` is the primary debugging tool. Any message in any queue directory — pending, processing, done, failed — is a self-contained Markdown document.

---

## 18. Cross-Compilation & Deployment

```bash
cargo build --release                                          # native
cross build --release --target armv7-unknown-linux-musleabihf   # RPi
cross build --release --target aarch64-unknown-linux-musl       # AArch64
cross build --release --target riscv64gc-unknown-linux-musl     # RISC-V
```

Deployment: copy one `apex` binary. Run `apex init`. Done.

---

## 19. Error Handling

| Error | Meaning |
|---|---|
| `QueueError::Empty` | No ready messages |
| `QueueError::Full` | Backpressure engaged |
| `LlmError::RateLimited { retry_after }` | Back off and retry |
| `LlmError::ContextOverflow { estimated, limit }` | Body + persona exceeded model window |
| `SandboxError::TimedOut { duration }` | Tool exceeded time limit |
| `ToolError::NotFound(name)` | Tool not in registry |
| `ToolError::OutputSpilled { path, size }` | Output exceeded budget (informational) |
| `ConfigError::ExceedsInvariant { field, requested, ceiling }` | Agent tried to exceed limits |
| `DepthError::MaxReached { current, max }` | Decomposition depth exceeded |
| `EvalError::DeterministicFailed(Vec<CriterionFailure>)` | Specific checks that failed |
| `EvalError::AdversarialFailed(Vec<BlockingIssue>)` | Adversarial eval found problems |
| `ComposerError::BodyExceedsBudget { tokens, budget }` | Message body too large to compose |

---

## 20. Observability

| What | How |
|---|---|
| Pending tasks | `ls queues/work/pending/` — each file is a complete task document |
| Active task | `cat queues/work/processing/*.md` — see what the agent is working on with full context |
| Completed tasks | `cat queues/work/done/*.md` — full execution narrative per task |
| Failed tasks | `cat queues/work/failed/*.md` — full failure narrative with all attempts |
| Job history | `grep -r "Correlation-Id: job-42" queues/work/done/` then `cat` each |
| Discovered facts | `grep -r "New Facts Discovered" queues/work/done/` |
| Working memory | `cat memory/working/job-42.md` — lightweight decomposition index |
| Long-term facts | `sqlite3 memory/long-term/apex.db "SELECT * FROM facts"` |
| Skill fitness | `sqlite3 ... "SELECT task_pattern, fitness FROM skills ORDER BY fitness DESC"` |
| Token calibration | `apex memory calibration` |
| Spilled outputs | `ls scratch/` |
| Context budget | Logged per iteration: body 5200t + persona 480t = 5680t / 6500t budget |
| Structured logs | JSON to stderr with Correlation-Id |

The primary observability surface is `cat` on message files. Every message in every queue state tells a complete story.

---

## 21. Resource Footprint

| Component | Size |
|---|---|
| apex binary (rfbmq-core + rusqlite + LTO + strip) | 3–6 MB |
| SQLite database | < 1 MB |
| Working memory per active job | < 10 KB (lightweight index) |
| Message files (pending + processing) | 2–8 KB each |
| Message files (done, before purge) | 2–8 KB each |
| Scratch files | Ephemeral, purged on ack |
| Runtime memory | ~15–35 MB |
| **Total** | **< 45 MB** |

Message bodies are larger than before (2-8 KB vs. ~200 bytes) but queue turnover is fast and `done/` is purged after consolidation. On tmpfs-constrained devices, tune `max_body_tokens` lower.

---

## 22. Open Design Questions

| # | Question | Options | Recommendation |
|---|---|---|---|
| 1 | rfbmq-core vendoring | Git subtree vs. path dep vs. crates.io | Path dependency, publish for releases |
| 2 | Polling vs. inotify | Poll with backoff vs. inotifywait | Poll for v1, inotify for v1.1 |
| 3 | Fact confidence decay | Time-based vs. usage-based | Time-based with configurable half-life |
| 4 | Concurrent loop instances | Threads vs. async tasks | Async tasks via tokio |
| 5 | Config hot-reload | Restart vs. SIGHUP | Restart for v1, SIGHUP for v1.1 |
| 6 | Done/ retention | Keep vs. auto-purge | Purge after consolidation, configurable retention window |
| 7 | Spill pattern detection | Regex set vs. fixed | Configurable regex set per tool |
| 8 | Token estimator scope | Per-model vs. global | Global for v1, per-model for v1.1 |
| 9 | FsyncMode default | Full vs. Batch | Batch for v1, explicit `apex queue sync` |
| 10 | Adversarial eval token budget | Shared vs. separate | Separate — eval shouldn't compete with execution context |
| 11 | update_body atomicity | Write-then-rename vs. in-place | Write-then-rename (consistent with rfbmq philosophy) |
| 12 | Message body format versioning | Implicit vs. explicit `Body-Version` header | Explicit header for forward compatibility |
| 13 | Continuation: read subtask results | Scan done/ vs. embed in continuation body | Scan done/ — subtask results may be large, embedding all would blow budget |
