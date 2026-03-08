use crate::estimator::TokenEstimator;
use apex_core::domain::{AttemptOutcome, AttemptRecord, Fact, Scratchpad, Skill};

pub struct MessageComposer;

impl MessageComposer {
    /// Compose the initial task body for a new queue message.
    pub fn compose_task_body(task: &str) -> String {
        let title = task.lines().next().unwrap_or(task);
        let title = if title.len() > 80 { &title[..80] } else { title };

        format!(
            "# Task: {title}\n\n\
             ## Description\n\
             {task}\n\n\
             ## Acceptance Criteria\n\
             (to be determined by agent)\n"
        )
    }

    /// Compose a success result narrative from an AttemptRecord.
    /// This replaces the message body before ack.
    pub fn compose_result(title: &str, record: &AttemptRecord) -> String {
        let mut out = format!("# Result: {title}\n\n## Outcome\nSUCCESS\n\n## Execution\n");

        let mut step = 1;
        for turn in &record.turns {
            for tc in &turn.tool_calls {
                let status = if tc.is_error { "ERROR" } else { "ok" };
                out.push_str(&format!(
                    "{}. Called `{}` — {} ({}ms, {})\n",
                    step, tc.name, tc.input_summary, tc.duration_ms, status
                ));
                step += 1;
            }
        }

        if step == 1 {
            out.push_str("(no tool calls)\n");
        }

        if let Some(ref text) = record.final_text {
            out.push_str(&format!("\n## Final Response\n{text}\n"));
        }

        out.push_str(&format!(
            "\n## Duration\n{} → {}\n",
            record.started_at, record.finished_at
        ));

        if let Some(ref eval) = record.eval_summary {
            out.push_str(&format!("\n## Evaluation\n{eval}\n"));
        }

        out
    }

    /// Append an attempt record to a message body for retry.
    /// Adds a "## Previous Attempts" section if not present,
    /// or appends to existing section.
    pub fn append_attempt(existing_body: &str, record: &AttemptRecord) -> String {
        let attempt_section = Self::format_attempt(record);

        if existing_body.contains("## Previous Attempts") {
            format!("{existing_body}\n{attempt_section}")
        } else {
            format!("{existing_body}\n## Previous Attempts\n{attempt_section}")
        }
    }

    /// Append an attempt record and working memory snapshot to a message body for retry.
    pub fn append_attempt_with_memory(
        existing_body: &str,
        record: &AttemptRecord,
        scratchpad: &Scratchpad,
    ) -> String {
        let with_attempt = Self::append_attempt(existing_body, record);
        format!(
            "{with_attempt}\n## Working Memory Snapshot\n{}\n",
            scratchpad.to_markdown()
        )
    }

    /// Compose a terminal failure narrative (all retries exhausted).
    pub fn compose_failure(title: &str, attempts: &[AttemptRecord]) -> String {
        let mut out = format!(
            "# Failed: {title}\n\n\
             ## Outcome\n\
             FAILED ({}/{} retries exhausted)\n\n\
             ## Attempt History\n",
            attempts.len(),
            attempts.len()
        );

        for attempt in attempts {
            out.push_str(&Self::format_attempt(attempt));
            out.push('\n');
        }

        out
    }

    /// Compose a subtask message body with embedded parent context.
    pub fn compose_subtask(
        title: &str,
        description: &str,
        acceptance_criteria: &str,
        parent_goal: &str,
        parent_context: &str,
    ) -> String {
        let budgeted_goal = TokenEstimator::budget(parent_goal, 500);
        let budgeted_context = TokenEstimator::budget(parent_context, 1000);

        format!(
            "# Subtask: {title}\n\n\
             ## Parent Goal\n\
             {budgeted_goal}\n\n\
             ## Context\n\
             {budgeted_context}\n\n\
             ## Task\n\
             {description}\n\n\
             ## Acceptance Criteria\n\
             {acceptance_criteria}\n"
        )
    }

    /// Compose a subtask message body with embedded parent context and long-term memory.
    pub fn compose_subtask_with_memory(
        title: &str,
        description: &str,
        acceptance_criteria: &str,
        parent_goal: &str,
        parent_context: &str,
        relevant_facts: &[Fact],
        recommended_skill: Option<&Skill>,
    ) -> String {
        let budgeted_goal = TokenEstimator::budget(parent_goal, 500);
        let budgeted_context = TokenEstimator::budget(parent_context, 1000);

        let mut out = format!(
            "# Subtask: {title}\n\n\
             ## Parent Goal\n\
             {budgeted_goal}\n\n\
             ## Context\n\
             {budgeted_context}\n\n"
        );

        if !relevant_facts.is_empty() {
            out.push_str("## Relevant Facts\n");
            for fact in relevant_facts {
                let budgeted = TokenEstimator::budget(&fact.content, 200);
                out.push_str(&format!(
                    "- [confidence: {:.2}] {budgeted}\n",
                    fact.confidence
                ));
            }
            out.push('\n');
        }

        if let Some(skill) = recommended_skill {
            out.push_str("## Recommended Approach\n");
            out.push_str(&format!("**Pattern:** {}\n", skill.task_pattern));
            let budgeted_approach = TokenEstimator::budget(&skill.approach, 300);
            out.push_str(&format!("**Approach:** {budgeted_approach}\n"));
            if !skill.tools_used.is_empty() {
                out.push_str(&format!("**Tools:** {}\n", skill.tools_used.join(", ")));
            }
            out.push_str(&format!("**Fitness:** {:.2}\n\n", skill.fitness));
        }

        // Determine acceptance criteria: use skill template if available and criteria is default
        let effective_criteria =
            if acceptance_criteria == "(to be determined by agent)" {
                if let Some(skill) = recommended_skill {
                    if let Some(ref template) = skill.criteria_template {
                        template.as_str()
                    } else {
                        acceptance_criteria
                    }
                } else {
                    acceptance_criteria
                }
            } else {
                acceptance_criteria
            };

        out.push_str(&format!(
            "## Task\n\
             {description}\n\n\
             ## Acceptance Criteria\n\
             {effective_criteria}\n"
        ));

        out
    }

    /// Compose a continuation message body that triggers result assembly.
    pub fn compose_continuation(
        correlation_id: &str,
        goal: &str,
        subtask_ids: &[String],
    ) -> String {
        let ids = subtask_ids.join(", ");
        format!(
            "# Continuation: {correlation_id}\n\n\
             ## Parent Goal\n\
             {goal}\n\n\
             ## Completed Subtask IDs\n\
             {ids}\n\n\
             ## Instructions\n\
             Read subtask results with queue_read_done, assemble final deliverable.\n"
        )
    }

    /// Compose a job-complete body summarizing all subtask results.
    pub fn compose_job_complete(
        title: &str,
        subtask_results: &[(String, String)], // (id, body)
    ) -> String {
        let mut out = format!(
            "# Result: {title}\n\n\
             ## Outcome\n\
             SUCCESS\n\n\
             ## Subtask Results\n"
        );

        for (id, body) in subtask_results {
            let summary = TokenEstimator::budget(body, 500);
            out.push_str(&format!("### {id}\n{summary}\n\n"));
        }

        out
    }

    /// Format a single attempt as markdown.
    fn format_attempt(record: &AttemptRecord) -> String {
        let outcome_str = match record.outcome {
            AttemptOutcome::Success => "SUCCESS",
            AttemptOutcome::Failed => "FAILED",
        };

        let mut out = format!("### Attempt {} — {outcome_str}\n", record.attempt_number);

        for turn in &record.turns {
            for tc in &turn.tool_calls {
                let status = if tc.is_error { "ERROR" } else { "ok" };
                out.push_str(&format!(
                    "- Called `{}` — {} ({}ms, {})\n",
                    tc.name, tc.input_summary, tc.duration_ms, status
                ));
                if tc.is_error && !tc.output_summary.is_empty() {
                    out.push_str(&format!("  Error: {}\n", tc.output_summary));
                }
            }
        }

        if let Some(ref reason) = record.failure_reason {
            out.push_str(&format!("- Diagnosis: {reason}\n"));
        }

        if let Some(ref eval) = record.eval_summary {
            out.push_str(&format!("- Evaluation:\n{eval}\n"));
        }

        if record.outcome == AttemptOutcome::Failed {
            if let Some(ref reason) = record.failure_reason {
                out.push_str(&format!("\n**→ Next attempt should address: {reason}**\n"));
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::{
        AttemptOutcome, AttemptRecord, Scratchpad, TokenUsage, ToolCallRecord, TurnRecord,
    };

    fn make_tool_call(name: &str, is_error: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: name.into(),
            input_summary: format!("{name} input"),
            output_summary: if is_error {
                "something went wrong".into()
            } else {
                "ok".into()
            },
            is_error,
            duration_ms: 100,
        }
    }

    fn make_turn(tool_calls: Vec<ToolCallRecord>) -> TurnRecord {
        TurnRecord {
            tool_calls,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
            },
        }
    }

    fn make_record(outcome: AttemptOutcome, attempt: u32) -> AttemptRecord {
        AttemptRecord {
            attempt_number: attempt,
            started_at: "2026-03-08T10:00:00Z".into(),
            finished_at: "2026-03-08T10:01:00Z".into(),
            turns: vec![make_turn(vec![
                make_tool_call("read_file", false),
                make_tool_call("write_file", false),
            ])],
            final_text: Some("All done.".into()),
            outcome: outcome.clone(),
            failure_reason: if outcome == AttemptOutcome::Failed {
                Some("file not found".into())
            } else {
                None
            },
            eval_summary: None,
        }
    }

    // ── compose_task_body ───────────────────────────────────────────

    #[test]
    fn compose_task_body_produces_correct_markdown() {
        let body = MessageComposer::compose_task_body("Refactor the parser\nMore details here.");
        assert!(body.starts_with("# Task: Refactor the parser\n"));
        assert!(body.contains("## Description\n"));
        assert!(body.contains("Refactor the parser\nMore details here."));
        assert!(body.contains("## Acceptance Criteria\n"));
        assert!(body.contains("(to be determined by agent)"));
    }

    #[test]
    fn compose_task_body_truncates_long_title() {
        let long_task = "A".repeat(120);
        let body = MessageComposer::compose_task_body(&long_task);
        let first_line = body.lines().next().unwrap();
        // "# Task: " is 9 chars, title should be 80 chars max
        assert!(first_line.len() <= 9 + 80);
    }

    // ── compose_result ──────────────────────────────────────────────

    #[test]
    fn compose_result_includes_tool_calls_and_timing() {
        let record = make_record(AttemptOutcome::Success, 1);
        let result = MessageComposer::compose_result("Parser refactor", &record);

        assert!(result.starts_with("# Result: Parser refactor\n"));
        assert!(result.contains("## Outcome\nSUCCESS"));
        assert!(result.contains("1. Called `read_file` — read_file input (100ms, ok)"));
        assert!(result.contains("2. Called `write_file` — write_file input (100ms, ok)"));
        assert!(result.contains("## Final Response\nAll done."));
        assert!(result.contains("## Duration\n2026-03-08T10:00:00Z → 2026-03-08T10:01:00Z"));
    }

    #[test]
    fn compose_result_no_tool_calls() {
        let record = AttemptRecord {
            attempt_number: 1,
            started_at: "t0".into(),
            finished_at: "t1".into(),
            turns: vec![],
            final_text: Some("Done without tools.".into()),
            outcome: AttemptOutcome::Success,
            failure_reason: None,
            eval_summary: None,
        };
        let result = MessageComposer::compose_result("Simple task", &record);
        assert!(result.contains("(no tool calls)"));
        assert!(result.contains("## Final Response\nDone without tools."));
    }

    #[test]
    fn compose_result_no_final_text() {
        let record = AttemptRecord {
            attempt_number: 1,
            started_at: "t0".into(),
            finished_at: "t1".into(),
            turns: vec![make_turn(vec![make_tool_call("bash", false)])],
            final_text: None,
            outcome: AttemptOutcome::Success,
            failure_reason: None,
            eval_summary: None,
        };
        let result = MessageComposer::compose_result("No text task", &record);
        assert!(!result.contains("## Final Response"));
    }

    // ── append_attempt ──────────────────────────────────────────────

    #[test]
    fn append_attempt_adds_section_on_first_call() {
        let body = "# Task: Do something\n\n## Description\nStuff.\n";
        let record = make_record(AttemptOutcome::Failed, 1);
        let result = MessageComposer::append_attempt(body, &record);

        assert!(result.contains("## Previous Attempts\n"));
        assert!(result.contains("### Attempt 1 — FAILED"));
        assert!(result.contains("- Diagnosis: file not found"));
        assert!(result.contains("**→ Next attempt should address: file not found**"));
    }

    #[test]
    fn append_attempt_appends_to_existing_section() {
        let record1 = make_record(AttemptOutcome::Failed, 1);
        let body_with_attempts = MessageComposer::append_attempt("# Task\n", &record1);

        let record2 = make_record(AttemptOutcome::Failed, 2);
        let result = MessageComposer::append_attempt(&body_with_attempts, &record2);

        // Should have exactly one "## Previous Attempts" header
        assert_eq!(result.matches("## Previous Attempts").count(), 1);
        // Should have both attempts
        assert!(result.contains("### Attempt 1 — FAILED"));
        assert!(result.contains("### Attempt 2 — FAILED"));
    }

    // ── compose_failure ─────────────────────────────────────────────

    #[test]
    fn compose_failure_includes_all_attempts() {
        let attempts = vec![
            make_record(AttemptOutcome::Failed, 1),
            make_record(AttemptOutcome::Failed, 2),
            make_record(AttemptOutcome::Failed, 3),
        ];
        let result = MessageComposer::compose_failure("Broken task", &attempts);

        assert!(result.starts_with("# Failed: Broken task\n"));
        assert!(result.contains("FAILED (3/3 retries exhausted)"));
        assert!(result.contains("### Attempt 1 — FAILED"));
        assert!(result.contains("### Attempt 2 — FAILED"));
        assert!(result.contains("### Attempt 3 — FAILED"));
    }

    // ── format_attempt (via compose_failure / append_attempt) ───────

    #[test]
    fn format_attempt_handles_error_tool_calls() {
        let record = AttemptRecord {
            attempt_number: 1,
            started_at: "t0".into(),
            finished_at: "t1".into(),
            turns: vec![make_turn(vec![make_tool_call("bash", true)])],
            final_text: None,
            outcome: AttemptOutcome::Failed,
            failure_reason: Some("command failed".into()),
            eval_summary: None,
        };
        let result = MessageComposer::append_attempt("# Task\n", &record);

        assert!(result.contains("- Called `bash` — bash input (100ms, ERROR)"));
        assert!(result.contains("  Error: something went wrong"));
        assert!(result.contains("- Diagnosis: command failed"));
    }

    // ── append_attempt_with_memory ─────────────────────────────────

    #[test]
    fn append_attempt_with_memory_includes_both() {
        let body = "# Task: Do something\n";
        let record = make_record(AttemptOutcome::Failed, 1);
        let pad = Scratchpad::new("job-42", "Do something");

        let result = MessageComposer::append_attempt_with_memory(body, &record, &pad);

        assert!(result.contains("## Previous Attempts"));
        assert!(result.contains("### Attempt 1 — FAILED"));
        assert!(result.contains("## Working Memory Snapshot"));
        assert!(result.contains("# Working Memory: job-42"));
        assert!(result.contains("## Goal\nDo something"));
    }

    #[test]
    fn format_attempt_success_no_failure_hint() {
        let record = make_record(AttemptOutcome::Success, 1);
        let result = MessageComposer::append_attempt("# Task\n", &record);

        assert!(result.contains("### Attempt 1 — SUCCESS"));
        assert!(!result.contains("**→ Next attempt should address"));
    }
}
