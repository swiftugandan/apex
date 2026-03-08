# The Apex Manifesto

*Principles for building AI agent harnesses that actually work.*

---

## 1. One loop is enough.

An agent is a loop: receive a task, think, act, evaluate, learn, repeat. Every "role" — planning, execution, evaluation, recovery, tool creation — is the same loop with different inputs. If you find yourself building separate orchestrators, evaluators, healers, and tool factories as distinct components with distinct code paths, you have confused configuration with architecture. A conductor is an agent with a planning persona. An evaluator is an agent with a critical persona. A healer is an agent with a diagnostic persona. The loop is the same. The persona, tools, and queue bindings are what differ. Build one loop. Parameterize it.

---

## 2. The message is the cognitive unit.

A task message is not a thin instruction. It is a self-contained document carrying everything the agent needs: the task description, its parent goal, relevant facts about the environment, the recommended approach, acceptance criteria, and the history of previous attempts. When you `cat` a message file in any queue directory — pending, processing, done, failed — you see the complete picture. What the agent knows, what it should do, how to verify success, and what's already been tried.

This means context assembly happens at push-time, not pop-time. The agent that creates a subtask embeds the relevant knowledge into the message body. The agent that receives it gets a ready-to-use prompt. No scattered lookups across databases and scratchpads at execution time. The message is the prompt.

---

## 3. Messages evolve through their lifecycle.

A message is not static. It is rewritten at every stage. When a task succeeds, the body becomes a complete execution narrative — what was done, what was verified, what was discovered. When a task fails, the body gains an attempt history with specific diagnoses and instructions for the next try. When retries are exhausted, the body becomes a failure report with root cause analysis.

The queue's `done/` directory is an audit trail of execution narratives. The `failed/` directory is a library of diagnosed failures. Both are readable with `cat`. The agent's history is not in logs or databases — it is in the messages themselves.

---

## 4. Memory is the architecture.

A stateless agent is a parlor trick. It solves the same problem the same way every time, rediscovering the same facts, repeating the same mistakes, never compounding. The architecture of an agent harness is not its orchestration topology or its pipeline stages — it is its memory system.

Long-term memory stores what the agent has learned across jobs: facts about the environment, skills that work with fitness scores, decomposition strategies with outcome data. This knowledge flows into message bodies at push-time — embedded into each subtask as relevant facts and recommended approaches. Working memory is a lightweight index tracking the decomposition state of the current job. The heavy cognitive context lives in the messages, not the scratchpad.

The first invocation and the hundredth invocation run the same loop. The difference is what's embedded in the messages. That difference is the product.

---

## 5. Budget context in tokens, at push-time.

Every token consumed by stale history, raw tool output, or boilerplate is a token unavailable for reasoning. Context management is a core architectural concern.

Budget in the unit the LLM consumes: tokens, not bytes. Use a self-calibrating estimator that learns the actual token ratios from every LLM response. Apply the budget when composing message bodies, not when popping them — each section of the message (facts, skill, criteria, attempt history) gets a token allocation, and the composer truncates to fit. The agent that pops the message just prepends its persona. Context is pre-budgeted.

Spill large tool outputs to disk. Present summaries. Let the agent drill into details with targeted follow-up queries. An agent operating on a compressed overview with the ability to drill will outperform an agent drowning in unfiltered context every time.

---

## 6. The filesystem is the debugger.

If you can't understand what your agent system is doing by running `ls` and `cat`, your architecture is hiding state in the wrong places.

Queue state is a directory listing. Every message in every queue directory is a complete, human-readable Markdown document — the task, its context, its execution narrative, its evaluation results. `cat` on a pending message shows what the agent will see. `cat` on a done message shows what happened. `cat` on a failed message shows what went wrong, every attempt, and why.

When an agent fails at 3am, the operator who debugs it will have `ssh`, `ls`, `cat`, and `grep`. The message bodies are the primary observability surface. Design for that operator.

---

## 7. Every function is a tool.

If the agent can invoke tools, and every capability is registered as a tool, then the agent can do anything the system can do. This includes modifying the system itself — creating new tools, adjusting configuration, storing knowledge, composing and pushing new messages.

Do not build special-purpose subsystems. Register them as tools with schemas. The agent invokes them the same way it invokes `shell_exec`. Uniformity of interface is uniformity of extensibility.

---

## 8. Evaluate with mechanisms, not opinions.

An LLM evaluating its own work has the same blind spots as when it produced the work. Self-evaluation through a single LLM call is confirmation bias with extra steps.

Use two layers. First: deterministic checks. Did the command exit 0? Does the file exist? Does the output contain the expected string? These are facts, not judgments. Second: adversarial review. A different persona — or better, a different model — whose explicit purpose is to find problems. The execution mind optimizes for success. The evaluation mind hunts for failure. Both are needed.

Push every criterion toward the deterministic layer. Reserve the adversarial layer for what genuinely requires reasoning. Write evaluation results into the message body so they're visible with `cat` and available as context for retries.

---

## 9. Declarations over code.

Tool definitions, policies, fitness thresholds, context budgets — all are data files. When behavior is data, changing behavior doesn't require recompilation. When behavior is data, the agent can change its own behavior at runtime. When behavior is data, validation is a function that reads files and checks consistency.

The operator sets invariants — hard ceilings the agent cannot exceed. Everything else is mutable configuration the agent adapts to its workload.

---

## 10. The agent should improve its own evaluation.

When a task succeeds, store the acceptance criteria that verified it alongside the skill record. When the same task pattern appears again, embed those proven criteria into the subtask message body. When a criterion catches a real failure, promote it. When a criterion never catches anything, it may be redundant.

Over time, evaluation quality improves without anyone writing better criteria by hand. The system learns not just how to do things, but how to verify that things are done correctly.

---

## 11. Fail forward with context.

When a task fails, the failure itself is the most valuable input for the retry. The message body is rewritten with the attempt history: what was tried, what went wrong, what the evaluator found, and what the next attempt should do differently.

An agent retrying with "previous attempt failed" is guessing. An agent retrying with a message body containing "attempt 1 failed because the script didn't handle spaces in filenames on line 12, as found by adversarial evaluation → next attempt should quote all variable expansions" is fixing a specific problem.

The message body is the mechanism. It accumulates context across retries. Each attempt makes the next attempt's prompt more informed.

---

## 12. Self-extension over escalation.

When the agent encounters a task it can't solve with existing tools, the default should be to build the tool, not to escalate to a human. Write the implementation, test it in the sandbox, register it, retry the task.

Escalation is appropriate when safety constraints are involved or when the problem genuinely exceeds the agent's capability. But "I don't have a tool for this" is a tool creation opportunity. The system's capability set should grow with use, not remain static.

---

## 13. Sandbox untrusted execution.

An agent that creates and runs arbitrary code must run that code in isolation. Read-only filesystem, isolated process tree, no network by default, memory and CPU limits enforced by the kernel.

The sandbox is not a security measure bolted on at the end. It is a prerequisite for self-extension. Without it, tool creation is a liability. With it, tool creation is the mechanism by which the system grows its own capabilities safely.

---

## 14. One binary, one deployment.

An agent harness for embedded devices cannot have runtime dependencies. No broker, no daemon, no container runtime, no language interpreter. A single static binary that you copy to the device and run.

Every external dependency is a deployment failure mode. Every daemon is a process to monitor. Compile everything in. Link the queue library. Bundle the database. Produce one file.

Deployment is: copy the file, run `init`, submit a goal.

---

## 15. Measure what compounds.

Track skill fitness: which approaches succeed and which fail, with rolling statistics. Track strategy fitness: which decomposition patterns produce the best outcomes. Track fact confidence: which knowledge is fresh and which is stale. Track criteria effectiveness: which acceptance checks catch real failures.

These metrics are not dashboards for human operators. They are inputs to the agent's own decision-making and flow into message bodies as recommended approaches and proven criteria. When the agent chooses an approach, it should choose the one with the highest empirical fitness, not the one that sounds best to the LLM in the moment.

The feedback loop from outcomes to fitness scores to future message bodies is the mechanism by which the system improves. Without it, you have an agent. With it, you have an agent that learns.

---

## 16. Transparency over cleverness.

Prefer a simple mechanism you can inspect over a clever one you can't. A Markdown message body is less sophisticated than a vector database, but you can read it with `cat` and it goes directly into the LLM as a prompt. A directory of message files is less performant than an in-memory event bus, but you can see the queue with `ls`. A TOML config file is less powerful than a programmatic API, but you can edit it with `vi`.

When the system is autonomous, the operator's ability to understand and intervene depends entirely on transparency. The message body is the prompt, the result narrative, the failure report, and the audit trail — all in one readable file. Every layer of abstraction that hides state is a layer that makes debugging harder.

---

*Build the simplest system that compounds.*
