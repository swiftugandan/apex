# Apex — Implementation Plan v2.0

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
