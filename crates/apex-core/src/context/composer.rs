use super::estimator::TokenEstimator;
use crate::domain::{AttemptOutcome, AttemptRecord, Fact, Scratchpad, Skill};

const MAX_TASK_TOKENS: u32 = 1000;
const MAX_FACTS_TOKENS: u32 = 1000;
const MAX_SKILL_TOKENS: u32 = 500;
const MAX_CRITERIA_TOKENS: u32 = 500;
const MAX_ATTEMPTS_TOKENS: u32 = 2000;

#[derive(Clone, Default)]
pub struct MessageComposer {
    estimator: TokenEstimator,
}

impl MessageComposer {
    pub fn new(estimator: TokenEstimator) -> Self {
        Self { estimator }
    }

    /// Compose the initial task body for a new queue message.
    pub fn compose_task_body(task: &str) -> String {
        let title = task.lines().next().unwrap_or(task);
        let title = crate::truncate_str(title, 80);

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

        // Scan for successful create_tool calls
        let mut new_tools: Vec<String> = Vec::new();
        for turn in &record.turns {
            for tc in &turn.tool_calls {
                if tc.name == "create_tool" && !tc.is_error {
                    new_tools.push(tc.input_summary.clone());
                }
            }
        }
        if !new_tools.is_empty() {
            out.push_str("\n## New Tools Created\n");
            for tool_info in &new_tools {
                out.push_str(&format!("- {tool_info}\n"));
            }
        }

        out
    }

    /// Append an attempt record to a message body for retry.
    pub fn append_attempt(&self, existing_body: &str, record: &AttemptRecord) -> String {
        let attempt_section = Self::format_attempt(record);

        let result = if existing_body.contains("## Previous Attempts") {
            format!("{existing_body}\n{attempt_section}")
        } else {
            format!("{existing_body}\n## Previous Attempts\n{attempt_section}")
        };

        self.compress_attempts(result)
    }

    /// Compress oldest attempts in the body to one-line summaries if over budget.
    fn compress_attempts(&self, body: String) -> String {
        let Some(section_start) = body.find("## Previous Attempts\n") else {
            return body;
        };
        let attempts_text = &body[section_start..];
        let attempts_tokens = self.estimator.estimate(attempts_text);

        if attempts_tokens <= MAX_ATTEMPTS_TOKENS {
            return body;
        }

        let prefix = &body[..section_start];

        let mut attempts: Vec<&str> = Vec::new();
        let content = &attempts_text["## Previous Attempts\n".len()..];
        let mut last_start = 0;
        for (i, _) in content.match_indices("### Attempt ") {
            if i > last_start && last_start > 0 {
                attempts.push(&content[last_start..i]);
            }
            last_start = i;
        }
        if last_start < content.len() {
            attempts.push(&content[last_start..]);
        }

        if attempts.len() <= 1 {
            return body;
        }

        let mut compressed = format!("{prefix}## Previous Attempts\n");
        for (i, attempt) in attempts.iter().enumerate() {
            if i < attempts.len() - 1 {
                let first_line = attempt.lines().next().unwrap_or("### Attempt ?");
                let outcome = if attempt.contains("SUCCESS") {
                    "SUCCESS"
                } else {
                    "FAILED"
                };
                let diagnosis = attempt
                    .lines()
                    .find(|l| l.starts_with("- Diagnosis: "))
                    .map(|l| l.trim_start_matches("- Diagnosis: "))
                    .or_else(|| {
                        attempt
                            .lines()
                            .find(|l| l.starts_with("- Called "))
                            .map(|l| l.trim_start_matches("- "))
                    })
                    .unwrap_or("(no details)");
                let num = first_line
                    .trim_start_matches("### Attempt ")
                    .split(' ')
                    .next()
                    .unwrap_or("?");
                compressed.push_str(&format!(
                    "### Attempt {num} [compressed]: {outcome} — {diagnosis}\n"
                ));
            } else {
                compressed.push_str(attempt);
            }
        }

        compressed
    }

    /// Append an attempt record and working memory snapshot to a message body for retry.
    pub fn append_attempt_with_memory(
        &self,
        existing_body: &str,
        record: &AttemptRecord,
        scratchpad: &Scratchpad,
    ) -> String {
        let with_attempt = self.append_attempt(existing_body, record);
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
        &self,
        title: &str,
        description: &str,
        acceptance_criteria: &str,
        parent_goal: &str,
        parent_context: &str,
    ) -> String {
        let budgeted_goal = self.estimator.budget(parent_goal, MAX_TASK_TOKENS);
        let budgeted_context = self.estimator.budget(parent_context, MAX_FACTS_TOKENS);

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
    #[allow(clippy::too_many_arguments)]
    pub fn compose_subtask_with_memory(
        &self,
        title: &str,
        description: &str,
        acceptance_criteria: &str,
        parent_goal: &str,
        parent_context: &str,
        relevant_facts: &[Fact],
        recommended_skill: Option<&Skill>,
    ) -> String {
        let budgeted_goal = self.estimator.budget(parent_goal, MAX_TASK_TOKENS);
        let budgeted_context = self.estimator.budget(parent_context, MAX_FACTS_TOKENS);

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
                let budgeted = self.estimator.budget(&fact.content, 200);
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
            let budgeted_approach = self.estimator.budget(&skill.approach, MAX_SKILL_TOKENS);
            out.push_str(&format!("**Approach:** {budgeted_approach}\n"));
            if !skill.tools_used.is_empty() {
                out.push_str(&format!("**Tools:** {}\n", skill.tools_used.join(", ")));
            }
            out.push_str(&format!("**Fitness:** {:.2}\n\n", skill.fitness));
        }

        let budgeted_criteria = self
            .estimator
            .budget(acceptance_criteria, MAX_CRITERIA_TOKENS);

        out.push_str(&format!(
            "## Task\n\
             {description}\n\n\
             ## Acceptance Criteria\n\
             {budgeted_criteria}\n"
        ));

        out
    }

    /// Format a slice of facts into a markdown section, capping total size by `max_tokens`.
    /// Each fact is budgeted to 200 tokens; facts are included in order until the section
    /// would exceed `max_tokens`. Used for JIT retrieval at claim start.
    pub fn format_facts_section(&self, facts: &[Fact], max_tokens: u32) -> String {
        const PER_FACT_TOKENS: u32 = 200;
        let header = "## Relevant facts (from long-term memory)\n\n";
        let header_tokens = self.estimator.estimate(header);
        let mut remaining_tokens = max_tokens.saturating_sub(header_tokens);
        let mut out = String::from(header);
        for fact in facts {
            let line = format!(
                "- [confidence: {:.2}] {}\n",
                fact.confidence,
                self.estimator.budget(&fact.content, PER_FACT_TOKENS)
            );
            let line_tokens = self.estimator.estimate(&line);
            if line_tokens > remaining_tokens {
                break;
            }
            remaining_tokens -= line_tokens;
            out.push_str(&line);
        }
        if out.len() == header.len() {
            return String::new();
        }
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
        &self,
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
            let summary = self.estimator.budget(body, MAX_SKILL_TOKENS);
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
    use crate::domain::{
        AttemptOutcome, AttemptRecord, Fact, FactId, Scratchpad, TokenUsage, ToolCallRecord,
        TurnRecord,
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
                ..Default::default()
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
        }
    }

    fn composer() -> MessageComposer {
        MessageComposer::default()
    }

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
        assert!(first_line.len() <= 9 + 80);
    }

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
        };
        let result = MessageComposer::compose_result("Simple task", &record);
        assert!(result.contains("(no tool calls)"));
        assert!(result.contains("## Final Response\nDone without tools."));
    }

    #[test]
    fn append_attempt_adds_section_on_first_call() {
        let c = composer();
        let body = "# Task: Do something\n\n## Description\nStuff.\n";
        let record = make_record(AttemptOutcome::Failed, 1);
        let result = c.append_attempt(body, &record);

        assert!(result.contains("## Previous Attempts\n"));
        assert!(result.contains("### Attempt 1 — FAILED"));
        assert!(result.contains("- Diagnosis: file not found"));
        assert!(result.contains("**→ Next attempt should address: file not found**"));
    }

    #[test]
    fn append_attempt_appends_to_existing_section() {
        let c = composer();
        let record1 = make_record(AttemptOutcome::Failed, 1);
        let body_with_attempts = c.append_attempt("# Task\n", &record1);

        let record2 = make_record(AttemptOutcome::Failed, 2);
        let result = c.append_attempt(&body_with_attempts, &record2);

        assert_eq!(result.matches("## Previous Attempts").count(), 1);
        assert!(result.contains("### Attempt 1"));
        assert!(result.contains("### Attempt 2"));
    }

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

    #[test]
    fn append_attempt_with_memory_includes_both() {
        let c = composer();
        let body = "# Task: Do something\n";
        let record = make_record(AttemptOutcome::Failed, 1);
        let pad = Scratchpad::new("job-42", "Do something");

        let result = c.append_attempt_with_memory(body, &record, &pad);

        assert!(result.contains("## Previous Attempts"));
        assert!(result.contains("### Attempt 1 — FAILED"));
        assert!(result.contains("## Working Memory Snapshot"));
        assert!(result.contains("# Working Memory: job-42"));
        assert!(result.contains("## Goal\nDo something"));
    }

    #[test]
    fn compose_subtask_uses_budgets() {
        let c = composer();
        let result = c.compose_subtask(
            "Build it",
            "Build the artifact",
            "Must compile",
            "Deploy the app",
            "Full context here",
        );
        assert!(result.contains("# Subtask: Build it"));
        assert!(result.contains("Deploy the app"));
        assert!(result.contains("Full context here"));
        assert!(result.contains("Build the artifact"));
        assert!(result.contains("Must compile"));
    }

    #[test]
    fn compose_job_complete_uses_budgets() {
        let c = composer();
        let results = vec![
            ("id-1".to_string(), "Result body 1".to_string()),
            ("id-2".to_string(), "Result body 2".to_string()),
        ];
        let result = c.compose_job_complete("Deploy", &results);
        assert!(result.contains("# Result: Deploy"));
        assert!(result.contains("### id-1"));
        assert!(result.contains("### id-2"));
    }

    #[test]
    fn format_facts_section_empty_returns_empty() {
        let c = composer();
        let result = c.format_facts_section(&[], 800);
        assert!(result.is_empty());
    }

    #[test]
    fn format_facts_section_includes_facts_within_budget() {
        let c = composer();
        let facts = vec![
            Fact {
                id: FactId("f1".into()),
                content: "Rust uses Cargo for builds".into(),
                source_job: "job-1".into(),
                confidence: 0.9,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            },
            Fact {
                id: FactId("f2".into()),
                content: "Tests live in the tests module".into(),
                source_job: "job-2".into(),
                confidence: 0.8,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            },
        ];
        let result = c.format_facts_section(&facts, 800);
        assert!(result.contains("## Relevant facts (from long-term memory)"));
        assert!(result.contains("Rust uses Cargo"));
        assert!(result.contains("Tests live"));
        assert!(result.contains("0.90"));
        assert!(result.contains("0.80"));
        let est = crate::context::TokenEstimator::default();
        assert!(est.estimate(&result) <= 800);
    }

    #[test]
    fn format_facts_section_respects_token_budget() {
        let c = composer();
        let long_content = "word ".repeat(200);
        let facts = vec![
            Fact {
                id: FactId("f1".into()),
                content: long_content.clone(),
                source_job: "j1".into(),
                confidence: 1.0,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            };
            5
        ];
        let result = c.format_facts_section(&facts, 50);
        let est = crate::context::TokenEstimator::default();
        let tokens = est.estimate(&result);
        assert!(
            tokens <= 50,
            "section should fit in 50 tokens, got {tokens}"
        );
    }
}
