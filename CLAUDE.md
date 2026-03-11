# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```sh
cargo check --workspace          # Fast compile check
cargo build -p apex-bin           # Build the CLI binary
cargo clippy --workspace          # Lint (must pass with zero warnings)
cargo fmt --all                   # Format code
cargo test --workspace            # Run all tests
cargo test -p apex-engine         # Test a single crate
cargo test -p apex-engine -- test_name  # Run a single test by name
```

## Architecture

Apex is a hexagonal-architecture agentic AI system. The engine is a pure orchestrator that depends only on trait abstractions — all concrete wiring happens in the binary crate.

### Crate Dependency Graph

```
apex-bin  (CLI, composition root)
  ├── apex-engine  (agentic loop, worker, generic registry)
  ├── apex-tools   (concrete ToolRegistry implementations)
  ├── apex-infra   (LLM, queue, memory adapters)
  └── apex-core    (port traits, domain types, config, errors)

apex-engine → apex-core only
apex-tools  → apex-core only
apex-infra  → apex-core only
```

**Critical invariant:** `apex-engine` must never depend on `apex-tools`, `apex-infra`, or `rfbmq-core`. Verify with `cargo tree -p apex-engine`.

### Key Abstractions (apex-core/src/ports.rs)

All cross-crate boundaries use `async_trait` traits: `LlmProvider`, `ToolRegistry`, `Queue`, `WorkingMemory`, `MemoryStore`, `SkillStore`, `HookRegistry`, `SubAgentSpawner`. Implementations are injected at the `apex-bin` composition root.

### Agentic Loop (apex-engine/src/agentic_loop.rs)

`run_agentic_loop()` drives LLM turns: request → tool calls → results → repeat. Returns `LoopOutcome` enum (`Completed`, `LlmError`, `Cancelled`, `TimedOut`, `BlockedByHook`, `MaxTurnsExhausted`). Auto-compaction triggers at 80% context window via `maybe_compact()`.

### Worker Loop (apex-engine/src/worker.rs)

`worker_loop()` pops tasks from the queue, builds per-claim tool registries via `ClaimToolFactory` trait, runs the agentic loop, and handles ack/nack/reject. `WorkerContext` carries all injected dependencies.

### Tool Composition (apex-bin/src/runtime/)

`build_static_tools()` composes 6 registries (Builtin, Memory, Custom, Config, Delegate, Hooks) into a single `CompositeToolRegistry` with O(1) dispatch. `CliClaimToolFactory` builds per-claim registries. `InProcessSpawner` handles sub-agent delegation.

### Hooks System

Event-driven lifecycle hooks loaded from `.apex/hooks/<event>.d/*.toml`. Events: `before_turn`, `after_turn`, `before_tool_call`, `after_tool_result`, `before_push`, `after_claim`, `on_success`, `on_failure`, `on_log`. Actions: script, transform, block, inject. Error classification is fully hook-driven (no hardcoded patterns).

### State & Config

- `ProjectPaths` (apex-engine/src/paths.rs): single source of truth for `.apex/` directory layout
- TOML config with serde defaults — partial config files work
- State sharing via `Arc<Mutex<T>>` across async boundaries
- `Scratchpad` (working memory) persisted as markdown files

### Design Principles (docs/MANIFESTO.md)

Single loop architecture, message-centric design, filesystem as primary debugger, token budgeting at push-time, hooks over hardcoded behavior, forward failure context through retries, one static binary deployment.
