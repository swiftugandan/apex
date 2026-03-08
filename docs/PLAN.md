# Apex — Implementation Plan v2.0

**Based on:** SAD v5.0 | **Date:** March 2026
**Approach:** Each phase ends with a working system that does something useful. No phase is scaffolding-only.

---

## Phase 1 — The Loop That Works

**Duration:** 5 days
**Deliverable:** An agent that accepts a task via stdin, calls an LLM with tools, executes tool calls, and prints the result. Single task, no queue, no memory, no evaluation. Works end-to-end on a real problem.

**What gets built:**

- `apex-core`: Domain types — `ToolDef`, `ToolResult`, `ToolSchema`, `CompletionRequest`, `CompletionResponse`, `ToolCompletionResponse` with `TokenUsage`. Port traits — `LlmProvider`, `ToolRegistry`.
- `apex-llm`: Anthropic Claude adapter via HTTP. Parses tool-use responses, returns `TokenUsage`.
- `apex-tools`: Three built-in tools — `shell_exec`, `file_read`, `file_write`. Each with JSON schema. `shell_exec` runs commands directly (no sandbox yet). Output truncated to `max_output` bytes (spill-to-disk deferred).
- `apex-bin`: Reads a task from stdin. Loads persona from `prompts/agent.md`. Calls LLM with persona + task as system prompt, tool schemas attached. Executes returned tool calls in a multi-turn loop (LLM may call tools, see results, call more tools). Prints the final text response.

**What you can do:**

```bash
echo "Find all files larger than 10MB in /var and list them by size" | apex run
```

The agent reasons about the task, calls `shell_exec` with `find /var -size +10M -exec ls -lh {} +`, returns the result. Single-shot, stateless, but the core loop works: persona → LLM → tool calls → result.

**Verification:** Run 5 diverse tasks — file operations, system inspection, script writing, multi-step tool use. All produce correct results.

---

## Phase 2 — The Queue and Message Bodies

**Duration:** 5 days
**Deliverable:** Tasks flow through an rfbmq queue as rich Markdown documents. Multiple tasks can be queued and processed sequentially. Completed tasks in `done/` are full execution narratives. Failed tasks land in `failed/` with attempt history.

**What gets built:**

- `vendor/rfbmq-core`: Integrate as path dependency.
- `apex-queue`: `RfbmqAdapter` implementing `ports::Queue`. Direct Rust calls to rfbmq-core — `enqueue()`, `dequeue()`, `ack()`, `nack()`. Custom headers: `Type`, `Correlation-Id`, `Depth`, `Retry-Count`.
- `update_body()`: Atomic write-then-rename to rewrite message body while preserving headers.
- `apex-context`: `MessageComposer` — initial version. `compose_result()` writes execution narrative into message body before ack. `append_attempt()` adds attempt history before nack. `compose_failure()` writes terminal failure narrative.
- `apex-bin`: `apex init` creates queue directory structure. `apex run` wraps the task in a message body with `## Acceptance Criteria` placeholder and pushes to `work/`. Agent loop polls `work/`, processes tasks, rewrites body with result narrative on success, appends attempt history on failure, dead-letters after `max_retries`.
- `apex cat` CLI command: reads and displays any message file.

**What you can do:**

```bash
apex init
apex run "install htop"
apex run "create a script that monitors disk usage"
apex queue                        # see depth
cat queues/work/pending/*.md      # see queued tasks
# ... wait for processing ...
cat queues/work/done/*.md         # full execution narratives
```

Each completed message in `done/` reads like a report:

```markdown
# Result: Install htop

## Outcome
SUCCESS

## Execution
1. Ran `apt-get update` — exit 0, 8s
2. Ran `apt-get install -y htop` — exit 0, 12s
3. Verified `which htop` → `/usr/bin/htop`

## Duration
20s
```

Kill the process mid-task, restart, `apex queue reap` reclaims the lease, task reprocesses.

**Verification:** Queue 10 tasks. Verify sequential processing. `cat` every done message — each is a complete narrative. Kill mid-task, verify reap + reprocessing. Verify failed messages in `failed/` include attempt history.

---

## Phase 3 — Working Memory and Multi-Step Tasks

**Duration:** 4 days
**Deliverable:** The agent maintains a lightweight per-job scratchpad tracking decomposition state. Multi-step tasks survive across loop iterations. Retries reference previous failures from the message body.

**What gets built:**

- `apex-memory`: `WorkingMemory` trait implementation. `Scratchpad` as lightweight Markdown at `memory/working/{job-id}.md`. Tracks: goal, decomposition status (task IDs, done/active/pending), job-level notes. No detailed execution logs — those live in message bodies.
- Agent loop integration: load scratchpad at step 2, update subtask status at step 10, save after every iteration.
- `working_memory_read` and `working_memory_update` tools.
- Retry flow now works end-to-end: on failure, `MessageComposer::append_attempt()` writes the failure details and a "Next attempt should..." line into the message body. On retry pop, the agent sees its own diagnosis as part of the prompt.

**What you can do:**

```bash
apex run "set up a cron job that backs up /var/data daily"
cat memory/working/job-*.md       # lightweight decomposition index
cat queues/work/done/*.md         # detailed execution per step
```

When a step fails, the retry message body contains:

```markdown
## Previous Attempts
### Attempt 1 — FAILED
- Ran `apt-get install awscli` — exit 100: package not found
- Diagnosis: apt sources not updated

**→ Next attempt should run `apt-get update` first.**
```

The agent reads this as part of its prompt and addresses the specific issue.

**Verification:** Submit a multi-step task. Verify scratchpad tracks status. Force a failure, verify the retry body contains the previous attempt and the agent addresses the specific finding.

---

## Phase 4 — Task Decomposition, Fan-Out, and Context Embedding

**Duration:** 6 days
**Deliverable:** The agent decomposes goals into subtask DAGs. Each subtask message carries its own embedded context — relevant facts, recommended approach, acceptance criteria. Independent subtasks run in parallel. Continuations assemble final results.

**What gets built:**

- Message types: `goal`, `task`, `subtask`, `continuation` via `Type` header.
- `Depends-On` header support in `apex-queue`. `Queue::ready()` returns messages whose dependencies are all in `done/`.
- `MessageComposer::compose_subtask()`: embeds parent goal context, relevant information, and acceptance criteria into each subtask body. Token budgeting — each section allocated within `max_body_tokens`.
- `TokenEstimator`: initial version with hardcoded ratios (4.0 prose, 3.0 code, 3.5 mixed). Calibration deferred to Phase 8.
- `decompose_goal` tool: agent calls this to break a goal into subtasks. Internally calls `MessageComposer` to embed context into each subtask body before pushing to queue.
- `MessageComposer::compose_continuation()`: creates continuation message with subtask IDs.
- `queue_read_done` tool: reads completed subtask results from `done/` by correlation ID.
- `MessageComposer::compose_job_complete()`: assembles final job narrative from subtask results.
- `Depth` header, enforced at `max_depth` (default 2).
- `max_concurrent` config: multiple async tokio tasks running the agent loop against the same queue.

**What you can do:**

```bash
apex run "set up monitoring with Prometheus and Grafana"
apex status                       # see task DAG
cat queues/work/pending/*.md      # each subtask is self-contained
```

Each subtask message carries everything the executing agent needs:

```markdown
# Task: Install Prometheus

## Parent Goal
Set up monitoring with Prometheus and Grafana

## Context
Subtask 1 of 5 in job-17. No dependencies.

## Acceptance Criteria
### Deterministic
- command: `prometheus --version`
  expect: exit_code 0
- command: `systemctl is-active prometheus`
  expect: output_contains "active"
```

Independent subtasks (install Prometheus, install Grafana) run in parallel across concurrent loop instances. The continuation fires when everything completes, reading results from `done/` and assembling the final narrative.

**Verification:** Submit a goal that decomposes into 4+ subtasks with diamond dependencies. Verify parallel execution of independent tasks. Verify correct ordering of dependent tasks. `cat` the continuation's done message — should be a complete job summary.

---

## Phase 5 — Deterministic Evaluation

**Duration:** 3 days
**Deliverable:** Tasks carry acceptance criteria in their message body. The agent checks its own work with executable tests after every task. Evaluation results are written into the result narrative.

**What gets built:**

- `apex-eval`: `Evaluator` struct with `run_deterministic()`. Parses `## Acceptance Criteria / ### Deterministic` section from message body.
- Check types: `exit_code`, `output_contains`, `output_matches`, `file_exists`, `file_contains`, `not_contains`, `http_status`, `json_path`, `file_size`.
- Agent loop integration at step 7: run deterministic checks after tool execution.
- `MessageComposer::compose_result()` updated: includes evaluation section in result narrative.
- `MessageComposer::append_attempt()` updated: includes which specific criterion failed.
- `decompose_goal` updated: agent attaches acceptance criteria to each subtask during composition. Persona encourages deterministic criteria wherever possible.

**What you can do:**

```bash
apex run "install nginx and verify it serves the default page"
cat queues/work/done/*.md
```

Result narrative includes:

```markdown
## Evaluation
### Deterministic: PASS (3/3)
- `which nginx` → exit 0 ✓
- `systemctl is-active nginx` → "active" ✓
- `curl -s localhost` → contains "Welcome to nginx" ✓
```

On failure, the specific failing criterion is in the retry message:

```markdown
### Attempt 1 — FAILED
- Deterministic eval: FAIL
  - `curl -s localhost` → exit 7: connection refused
  - `systemctl is-active nginx` → "inactive"
- Diagnosis: nginx installed but not started

**→ Next attempt should start nginx after installation.**
```

**Verification:** Submit a task with criteria that fail on first attempt. Verify the agent detects failure via criteria, records it in the body, and fixes it on retry. Verify the done narrative shows pass/fail per criterion.

---

## Phase 6 — Adversarial Evaluation

**Duration:** 3 days
**Deliverable:** Fuzzy acceptance criteria trigger a second LLM call with an adversarial persona. Specific findings flow into the message body for retry context.

**What gets built:**

- `prompts/evaluator.md`: adversarial persona focused on finding problems.
- `apex-eval`: `run_adversarial()` — second LLM call with evaluation persona.
- Fuzzy criteria parser: `### Fuzzy` section in acceptance criteria.
- `EvalConfig`: `eval_model` (can differ from execution model), `eval_on` (always / fuzzy_criteria / never).
- `Evaluation` struct: combines deterministic and adversarial results with `blocking_issues` and `warnings`.
- `MessageComposer` integration: adversarial findings written into result narratives and attempt histories.

**What you can do:**

```bash
apex run "write a bash script that safely rotates log files"
cat queues/work/done/*.md
```

Result narrative:

```markdown
## Evaluation
### Deterministic: PASS (2/2)
- script exists and is executable ✓
- shellcheck passes ✓

### Adversarial: PASS
No blocking issues.
Warnings: script doesn't compress rotated logs (space consideration).
```

On adversarial failure:

```markdown
### Attempt 1 — FAILED
- Deterministic: PASS (2/2)
- Adversarial: FAIL
  - Blocking: script doesn't use flock — concurrent cron runs
    could corrupt logs
  - Blocking: no check for log directory existence

**→ Next attempt should add flock wrapper and directory check.**
```

**Verification:** Submit a task with fuzzy criteria. Verify adversarial eval catches an issue deterministic checks missed. Verify retry addresses the specific finding. Test with `eval_model` different from execution model.

---

## Phase 7 — Long-Term Memory and Consolidation

**Duration:** 5 days
**Deliverable:** Facts, skills, and strategies persist across jobs. Context from long-term memory is embedded into subtask message bodies at push-time. The agent gets better at repeated task types.

**What gets built:**

- `apex-memory`: SQLite tables for `facts`, `skills`, `strategies`. `MemoryStore` trait implementation.
- Consolidation: when a `continuation` fires, read `## New Facts Discovered` and `## Skills Updated` from subtask result bodies in `done/`. Extract facts, update skill fitness, update strategy fitness, record new skills.
- `criteria_template` on skills: proven acceptance criteria stored and reused when composing subtask bodies for matching task patterns.
- `MessageComposer::compose_subtask()` updated: queries `MemoryStore` for relevant facts and best-fit skill. Embeds `## Relevant Facts` and `## Recommended Approach` sections into subtask body. Includes skill's `criteria_template` in acceptance criteria section.
- `MessageComposer::compose_result()` updated: includes `## New Facts Discovered` section.
- Memory tools: `memory_store_fact`, `memory_query_facts`, `memory_store_skill`, `memory_query_skill`, `memory_store_strategy`.
- Fact confidence decay: time-based, configurable half-life.
- Skill fitness: `success_count / (success_count + failure_count)` after `min_samples`. `auto_retire_below` threshold.

**What you can do:**

```bash
# First run
apex run "set up S3 backup for /var/data"
# Agent discovers: no Python, credentials at /etc/aws/credentials
# Consolidation extracts facts, records skills

# Second run, weeks later
apex run "set up S3 backup for /opt/reports"
cat queues/work/pending/*.md
```

The subtask message body now includes:

```markdown
## Relevant Facts
- Device runs Debian 11, no Python installed
- AWS credentials at /etc/aws/credentials
- awscli 1.22.34 installed via apt

## Recommended Approach
Skill: s3-backup-awscli (fitness: 0.88, 3 uses)
Use aws s3 sync. Verify with --dry-run before scheduling.

## Acceptance Criteria
### Deterministic (from criteria template)
- command: `bash /opt/backup.sh --dry-run`
  expect: exit_code 0
- command: `bash /opt/backup.sh --dry-run`
  expect: output_contains "upload:"
```

The second run skips the boto3 dead end entirely. Uses proven criteria from the first run. Completes faster with fewer retries.

```bash
apex memory facts                 # accumulated knowledge
apex memory skills                # skills with fitness
apex memory strategies            # decomposition patterns
```

**Verification:** Run the same class of task 3 times. Verify the third run embeds facts and skills from the first two into subtask bodies. Measure: fewer retries, fewer LLM calls, faster completion.

---

## Phase 8 — Token Calibration and Output Spill

**Duration:** 4 days
**Deliverable:** Token estimator self-calibrates from LLM responses. Message body composition is token-accurate. Tool output spills to disk with summary envelopes. The agent handles large outputs gracefully.

**What gets built:**

- `apex-context`: `TokenEstimator` with calibration from `response.usage.prompt_tokens`. Exponential moving average per content type. Calibration persisted in SQLite `calibration` table. Loaded on startup.
- `MessageComposer` updated: all section allocations use `TokenEstimator` instead of character heuristics. Attempt history compression — oldest attempts collapse to one-line summaries when `max_attempts_tokens` is exceeded.
- Tool output spill: when output exceeds `max_output` bytes, write to `scratch/`, return summary envelope with head/tail/stats per `spill_strategy`.
- `ToolResult` struct: `stdout`, `stderr`, `spill_path`, `stats`, `truncated`, `duration`.
- `OutputStats`: `total_lines`, `total_bytes`, `patterns`.
- Pre-filtering on `shell_exec`: `grep`, `tail`, `max_lines` parameters.
- `scratch/` lifecycle: create on spill, delete on ack, clean on reap.
- `apex scratch ls` and `apex memory calibration` CLI commands.

**What you can do:**

```bash
apex run "find and analyze all error patterns in /var/log/syslog"
# syslog is 500MB. Agent uses grep filter.
# If output still large, spills to scratch/
# Agent receives summary, drills in with follow-ups

apex memory calibration           # see estimator accuracy
# prose: 3.87 chars/token (calibrated from 47 samples)
# code: 2.93 chars/token
# mixed: 3.41 chars/token
```

After 20-30 LLM calls, token estimates are within ~5%. Message body composition is accurate — no more overflowing the model's context window or underusing it.

**Verification:** Trigger a tool output >1MB. Verify spill, summary envelope, and drill-down. Run 30 tasks, verify calibration converges. Verify message body token estimates match actual usage within 10%.

---

## Phase 9 — Sandbox

**Duration:** 5 days
**Deliverable:** Tool execution runs in Linux namespace isolation. The agent can safely execute untrusted code, including agent-created tools.

**What gets built:**

- `apex-sandbox`: Linux namespace sandbox using `nix` crate. Mount namespace (read-only root, writable tmpfs), PID namespace, network namespace (disabled by default, opt-in per tool), seccomp filter, cgroups (memory + CPU limits), UID mapping.
- `SandboxCommand`, `SandboxResult` with resource usage reporting.
- `NoopSandbox` adapter for limited-namespace devices.
- Per-tool sandbox config in manifest: `sandbox = true/false`, `network = true/false`.
- `sandbox_exec` tool for explicit sandboxed execution.
- `shell_exec` routes through sandbox when `sandbox = true` in manifest.
- Timeout enforcement via cgroup CPU limits.

**What you can do:**

```bash
apex run "write and test a Python script that processes CSV data"
# Agent writes script, tests in sandbox
# Script can't access host filesystem beyond workspace
# Script can't use network (unless tool opts in)
# Script killed after 30s or 256MB
```

This makes Phase 10 (tool creation) safe.

**Verification:** Tool writes to `/etc/` → permission denied. Tool allocates 1GB → killed by cgroup. Tool curls external URL with `network = false` → fails. Tool runs for 60s → killed by timeout.

---

## Phase 10 — Dynamic Tool Creation

**Duration:** 4 days
**Deliverable:** The agent creates new tools at runtime — writes the implementation, tests in sandbox, registers in manifest, uses immediately. Tool creation events appear in result narratives.

**What gets built:**

- `create_tool` tool implementation: accepts name, description, approach. LLM generates script/binary. Written to `tools/custom/{name}/`. Schema generated. Tested in sandbox with LLM-generated tests. Entry appended to `tools/manifest.toml`.
- Skill linkage: `create_tool` creates a skill record linking task pattern to tool.
- `MessageComposer` integration: when a result narrative includes tool creation, the "New Facts Discovered" section mentions the new tool.
- Validation: schema parses, implementation passes tests, description non-empty.

**What you can do:**

```bash
apex run "monitor disk usage and alert when any partition exceeds 80%"
# Agent lacks a monitoring tool
# Creates tools/custom/disk-monitor/
# Tests it in sandbox
# Registers in manifest
# Uses it to complete the task

apex tools list                   # disk-monitor appears
cat queues/work/done/*.md         # narrative includes tool creation

# Future tasks can use disk-monitor directly
```

**Verification:** Submit task requiring missing capability. Verify tool creation, testing, registration, and use. Submit second task needing same capability — verify it reuses the tool.

---

## Phase 11 — Self-Modifying Configuration

**Duration:** 3 days
**Deliverable:** The agent modifies its own config within operator invariants. Validation prevents exceeding ceilings.

**What gets built:**

- `config/invariants.toml`: immutable operator ceilings.
- `update_config` tool: read, validate against invariants, write.
- `apex validate` CLI: checks all config, manifest, and invariant consistency.

**What you can do:**

```bash
apex run "process these 200 log files in parallel"
# Agent raises max_concurrent from 4 to 8
# Processes across 8 loop instances
# Lowers it back after

apex config show                  # see current config
apex validate                     # check consistency
```

**Verification:** Agent sets value above invariant ceiling → error. Agent sets valid value → takes effect. `apex validate` catches inconsistencies.

---

## Phase 12 — Polish and Hardening

**Duration:** 4 days
**Deliverable:** Production-ready single binary. Graceful shutdown, structured logging, cross-compilation, documentation.

**What gets built:**

- Graceful shutdown: SIGTERM drains current tasks, NACKs in-progress, exits cleanly.
- Structured JSON logging to stderr with `Correlation-Id`.
- Cross-compilation: build and test on ARM, AArch64, RISC-V.
- Binary size: LTO + strip, verify < 6MB.
- `apex init` creates full directory structure with default configs, personas, manifests.
- `apex version`.
- README, deployment guide, operator runbook.
- End-to-end integration test: complex goal → decompose → fan-out → execute → evaluate (both layers) → consolidate → memory updated → second similar goal uses learned knowledge.

**What you can do:**

```bash
cross build --release --target armv7-unknown-linux-musleabihf
scp target/armv7-unknown-linux-musleabihf/release/apex pi@device:~/
ssh pi@device
./apex init
./apex run "set up this device as a temperature monitoring station"
# Fully autonomous: decomposes, executes, evaluates, learns
```

**Verification:** End-to-end on two architectures. Binary < 6MB. Graceful shutdown preserves queue integrity. 24-hour soak test.

---

## Summary

| Phase | Days | Cumulative | Deliverable |
|---|---|---|---|
| 1. The Loop | 5 | 5 | stdin → LLM → tools → result |
| 2. Queue + Message Bodies | 5 | 10 | rfbmq queue, rich Markdown narratives in done/failed |
| 3. Working Memory | 4 | 14 | Per-job scratchpad, retry with failure context in body |
| 4. Decomposition + Context Embedding | 6 | 20 | Task DAGs, self-contained subtask bodies, fan-out |
| 5. Deterministic Eval | 3 | 23 | Acceptance criteria, executable checks in body |
| 6. Adversarial Eval | 3 | 26 | Second-opinion LLM, findings in body for retry |
| 7. Long-Term Memory | 5 | 31 | Facts/skills/strategies, embedded in subtask bodies |
| 8. Token Calibration + Spill | 4 | 35 | Self-calibrating estimator, output spill, accurate budgets |
| 9. Sandbox | 5 | 40 | Namespace isolation, safe execution |
| 10. Tool Creation | 4 | 44 | Self-extending: creates and registers tools |
| 11. Self-Config | 3 | 47 | Agent modifies config within guardrails |
| 12. Polish | 4 | 51 | Production binary, cross-compilation, hardening |

**Total: 51 days.** After Phase 1 (day 5), you have a working agent. After Phase 2 (day 10), every task produces a readable Markdown narrative. After Phase 7 (day 31), the system compounds. Every phase builds on the last, and every phase delivers something you can use and inspect with `cat`.
