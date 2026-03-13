use apex_core::config::ConsolidationSection;
use apex_core::domain::{
    slugify, AttemptOutcome, AttemptRecord, CacheHint, ChatMessage, CompletionRequest, Fact,
    FactId, Scratchpad, Skill, SkillId, SkillStatus, SubtaskStatus, SystemBlock,
};
use apex_core::ports::{HookRegistry, LlmProvider, MemoryStore, SkillStore};

use crate::log::dispatch_log;

async fn log_consolidation_err(hooks: Option<&dyn HookRegistry>, context: &str, error: &str) {
    let fallback = format!("  consolidation: {context}: {error}");
    dispatch_log(
        hooks,
        || {
            serde_json::json!({
                "level": "warn",
                "event": "consolidation_error",
                "context": context,
                "error": error,
            })
        },
        &fallback,
    )
    .await;
}

/// Maximum chars for the tool trace sent to the LLM for skill extraction.
const MAX_TOOL_TRACE_CHARS: usize = 4000;

/// Build a compact one-line-per-tool-call trace from attempt turns.
fn build_tool_trace(record: &AttemptRecord) -> String {
    let mut trace = String::new();
    for turn in &record.turns {
        for tc in &turn.tool_calls {
            let line = format!("{}({})\n", tc.name, tc.input_summary);
            if trace.len() + line.len() > MAX_TOOL_TRACE_CHARS {
                trace.push_str("...(truncated)\n");
                return trace;
            }
            trace.push_str(&line);
        }
    }
    trace
}

/// LLM-extracted skill fields: (name, description, approach).
async fn extract_skill_via_llm(
    llm: &dyn LlmProvider,
    skill_store: &dyn SkillStore,
    goal: &str,
    record: &AttemptRecord,
) -> Option<(String, String, String)> {
    let tool_trace = build_tool_trace(record);

    // Gather existing skill names for deduplication hints.
    let existing_names: Vec<String> = skill_store
        .list_manifests()
        .await
        .ok()?
        .into_iter()
        .map(|m| m.name)
        .collect();

    let existing_list = if existing_names.is_empty() {
        "(none)".to_string()
    } else {
        existing_names.join(", ")
    };

    let prompt = format!(
        r#"You are an AI skill cataloger. Given a completed task and its tool trace, produce a reusable skill entry.

Task goal: {goal}

Tool trace:
{tool_trace}

Existing skill names (reuse one if this task matches): {existing_list}

Return ONLY a JSON object with these fields:
- "name": a generalized, lowercase-hyphenated skill name (max 64 chars). Example: "analyze-codebase-architecture". Reuse an existing name if the task is essentially the same skill.
- "description": imperative phrasing starting with "Use this skill when...", max 1024 chars.
- "approach": numbered step-by-step instructions an agent can follow to reproduce this skill. Be specific about which tools to use and in what order.

JSON only, no markdown fences."#
    );

    let messages = vec![ChatMessage::user_text(&prompt)];
    let system_blocks = [SystemBlock {
        text: "You are a concise JSON generator. Output valid JSON only.".to_string(),
        cache_hint: CacheHint::Dynamic,
    }];

    let req = CompletionRequest {
        system_blocks: &system_blocks,
        messages: &messages,
        max_tokens: 16000,
        temperature: Some(0.0),
        cache_tools: false,
        reserved_reasoning_tokens: 0,
    };

    let resp = llm.complete(req).await.ok()?;
    let text = resp.text();
    if text.is_empty() {
        return None;
    }

    // Parse JSON — try stripping markdown fences as safety net.
    let json_str = text
        .trim()
        .strip_prefix("```json")
        .or_else(|| text.trim().strip_prefix("```"))
        .unwrap_or(text.trim())
        .strip_suffix("```")
        .unwrap_or(text.trim())
        .trim();

    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let name = slugify(parsed.get("name")?.as_str()?);
    let description = parsed.get("description")?.as_str()?.to_string();
    // Accept approach as either a string or an array of strings.
    let approach = match parsed.get("approach")? {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };

    if name.is_empty() || description.is_empty() || approach.is_empty() {
        return None;
    }

    // Enforce max lengths.
    let name = if name.len() > 64 {
        name[..64].trim_end_matches('-').to_string()
    } else {
        name
    };
    let description = if description.len() > 1024 {
        description[..1024].to_string()
    } else {
        description
    };

    Some((name, description, approach))
}

/// Best-effort post-execution learning extraction.
#[allow(clippy::too_many_arguments)]
pub async fn consolidate_learnings(
    store: &dyn MemoryStore,
    skill_store: &dyn SkillStore,
    llm: &dyn LlmProvider,
    correlation_id: &str,
    record: &AttemptRecord,
    scratchpad: &Scratchpad,
    config: &ConsolidationSection,
    hooks: Option<&dyn HookRegistry>,
) {
    // 1. Extract facts from "## New Facts Discovered" sections
    if config.extract_facts {
        if let Some(ref text) = record.final_text {
            let mut in_facts_section = false;
            for line in text.lines() {
                if line.contains("New Facts Discovered") || line.contains("new facts discovered") {
                    in_facts_section = true;
                    continue;
                }
                if in_facts_section && line.starts_with("## ") {
                    break;
                }
                if in_facts_section {
                    if let Some(content) = line.strip_prefix("- ") {
                        let content = content.trim();
                        if !content.is_empty() {
                            let fact = Fact {
                                id: FactId(String::new()),
                                content: content.to_string(),
                                source_job: correlation_id.to_string(),
                                confidence: 0.8,
                                created_at: String::new(),
                                last_verified: String::new(),
                                tags: vec![],
                            };
                            if let Err(e) = store.store_fact(fact).await {
                                log_consolidation_err(
                                    hooks,
                                    "failed to store fact",
                                    &e.to_string(),
                                )
                                .await;
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Skills: update fitness or create new skill
    if config.extract_skills {
        let title = &scratchpad.goal;
        if !title.is_empty() && record.outcome == AttemptOutcome::Success {
            // Try LLM-powered extraction first (if enabled), fall back to deterministic.
            let extracted = if config.use_llm_extraction {
                extract_skill_via_llm(llm, skill_store, title, record).await
            } else {
                None
            };

            let (skill_name, skill_description, skill_approach) = match extracted {
                Some((name, desc, approach)) => (name, desc, approach),
                None => {
                    // Deterministic fallback.
                    let approach = record
                        .final_text
                        .as_deref()
                        .unwrap_or("")
                        .lines()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ");
                    (slugify(title), title.to_string(), approach)
                }
            };

            match skill_store.load_skill(&skill_name, "latest").await {
                Ok(Some(skill)) => {
                    if let Err(e) = skill_store.update_skill_fitness(&skill.id, true).await {
                        log_consolidation_err(
                            hooks,
                            "failed to update skill fitness",
                            &e.to_string(),
                        )
                        .await;
                    }
                }
                Ok(None) => {
                    let tools_used: Vec<String> = record
                        .turns
                        .iter()
                        .flat_map(|t| t.tool_calls.iter())
                        .map(|tc| tc.name.clone())
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();

                    if !tools_used.is_empty() {
                        let skill = Skill {
                            id: SkillId(String::new()),
                            name: skill_name,
                            description: skill_description,
                            license: None,
                            compatibility: None,
                            allowed_tools: None,
                            extra_metadata: Default::default(),
                            task_pattern: title.to_string(),
                            approach: skill_approach,
                            tools_used,
                            success_count: 1,
                            failure_count: 0,
                            fitness: 0.5,
                            min_samples: 3,
                            last_used: String::new(),
                            status: SkillStatus::Active,
                            version: "1.0.0".to_string(),
                            skill_dir: None,
                        };
                        if let Err(e) = skill_store.store_skill(skill).await {
                            log_consolidation_err(hooks, "failed to store skill", &e.to_string())
                                .await;
                        }
                    }
                }
                Err(e) => {
                    log_consolidation_err(hooks, "failed to find skill", &e.to_string()).await;
                }
            }
        } else if !title.is_empty() {
            // Failed task: still update fitness if skill exists.
            let skill_name = slugify(title);
            if let Ok(Some(skill)) = skill_store.load_skill(&skill_name, "latest").await {
                if let Err(e) = skill_store.update_skill_fitness(&skill.id, false).await {
                    log_consolidation_err(hooks, "failed to update skill fitness", &e.to_string())
                        .await;
                }
            }
        }
    }

    // 3. Decomposition skills: for jobs with subtasks, store as a skill
    if config.extract_strategies && !scratchpad.subtasks.is_empty() && !scratchpad.goal.is_empty() {
        let decomposition = scratchpad
            .subtasks
            .iter()
            .map(|st| format!("{}. {}", st.index, st.description))
            .collect::<Vec<_>>()
            .join("\n");

        let decompose_name = slugify(&format!("decompose-{}", scratchpad.goal));
        match skill_store.load_skill(&decompose_name, "latest").await {
            Ok(Some(skill)) => {
                let success = scratchpad
                    .subtasks
                    .iter()
                    .all(|st| st.status == SubtaskStatus::Done);
                if let Err(e) = skill_store.update_skill_fitness(&skill.id, success).await {
                    log_consolidation_err(
                        hooks,
                        "failed to update decomposition skill fitness",
                        &e.to_string(),
                    )
                    .await;
                }
            }
            Ok(None) => {
                let extra_metadata = std::collections::BTreeMap::from([(
                    "apex-avg-subtasks".to_string(),
                    scratchpad.subtasks.len().to_string(),
                )]);
                let skill = Skill {
                    id: SkillId(String::new()),
                    name: slugify(&format!("decompose-{}", scratchpad.goal)),
                    description: format!("Decomposition strategy for: {}", scratchpad.goal),
                    license: None,
                    compatibility: None,
                    allowed_tools: None,
                    extra_metadata,
                    task_pattern: format!("decompose: {}", scratchpad.goal),
                    approach: decomposition,
                    tools_used: vec![],
                    success_count: if record.outcome == AttemptOutcome::Success {
                        1
                    } else {
                        0
                    },
                    failure_count: if record.outcome == AttemptOutcome::Failed {
                        1
                    } else {
                        0
                    },
                    fitness: 0.5,
                    min_samples: 3,
                    last_used: String::new(),
                    status: SkillStatus::Active,
                    version: "1.0.0".to_string(),
                    skill_dir: None,
                };
                if let Err(e) = skill_store.store_skill(skill).await {
                    log_consolidation_err(
                        hooks,
                        "failed to store decomposition skill",
                        &e.to_string(),
                    )
                    .await;
                }
            }
            Err(e) => {
                log_consolidation_err(hooks, "failed to find decomposition skill", &e.to_string())
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::config::ConsolidationSection;
    use apex_core::domain::{
        AttemptOutcome, AttemptRecord, CompletionResponse, Skill, SkillId, SkillStatus, StopReason,
        TokenUsage, ToolCallRecord, TurnRecord,
    };
    use apex_core::error::LlmError;
    use apex_core::ports::LlmProvider;
    use async_trait::async_trait;

    use crate::test_mocks::{MockMemoryStore, MockSkillStore};

    // ── Mock LLM for consolidation tests ────────────────────────────

    struct MockConsolidationLlm {
        response: Result<String, LlmError>,
    }

    impl MockConsolidationLlm {
        fn success(text: &str) -> Self {
            Self {
                response: Ok(text.to_string()),
            }
        }

        fn error() -> Self {
            Self {
                response: Err(LlmError::Api("mock error".into())),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockConsolidationLlm {
        async fn complete(
            &self,
            _req: CompletionRequest<'_>,
        ) -> Result<CompletionResponse, LlmError> {
            match &self.response {
                Ok(text) => Ok(CompletionResponse {
                    message: ChatMessage {
                        role: apex_core::domain::MessageRole::Assistant,
                        content: vec![apex_core::domain::ContentBlock::Text { text: text.clone() }],
                    },
                    usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                        ..Default::default()
                    },
                    stop_reason: StopReason::EndTurn,
                }),
                Err(e) => Err(LlmError::Api(e.to_string())),
            }
        }

        async fn complete_with_tools(
            &self,
            _req: CompletionRequest<'_>,
            _tools: &[apex_core::domain::ToolSchema],
        ) -> Result<apex_core::domain::ToolCompletionResponse, LlmError> {
            unimplemented!("not needed for consolidation tests")
        }

        fn model_id(&self) -> &str {
            "mock-model"
        }
        fn context_window(&self) -> usize {
            200_000
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────

    fn make_record_with_tools(tool_names: &[&str]) -> AttemptRecord {
        let tool_calls: Vec<ToolCallRecord> = tool_names
            .iter()
            .map(|name| ToolCallRecord {
                name: name.to_string(),
                input_summary: "test input".to_string(),
                output_summary: "test output".to_string(),
                is_error: false,
                duration_ms: 100,
            })
            .collect();

        AttemptRecord {
            attempt_number: 1,
            started_at: String::new(),
            finished_at: String::new(),
            outcome: AttemptOutcome::Success,
            final_text: Some("Task completed successfully.".to_string()),
            turns: vec![TurnRecord {
                tool_calls,
                usage: TokenUsage::default(),
            }],
            failure_reason: None,
        }
    }

    fn make_config(use_llm: bool) -> ConsolidationSection {
        ConsolidationSection {
            use_llm_extraction: use_llm,
            ..ConsolidationSection::default()
        }
    }

    fn make_scratchpad(goal: &str) -> apex_core::domain::Scratchpad {
        apex_core::domain::Scratchpad::new("test-job", goal)
    }

    fn make_existing_skill(name: &str) -> Skill {
        Skill {
            id: SkillId("existing-1".to_string()),
            name: name.to_string(),
            description: "Existing skill".to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            extra_metadata: Default::default(),
            task_pattern: "existing pattern".to_string(),
            approach: "existing approach".to_string(),
            tools_used: vec!["shell_exec".to_string()],
            success_count: 3,
            failure_count: 1,
            fitness: 0.75,
            min_samples: 3,
            last_used: String::new(),
            status: SkillStatus::Active,
            version: "1.0.0".to_string(),
            skill_dir: None,
        }
    }

    // ── Tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn llm_returns_valid_json_extracts_skill() {
        let llm = MockConsolidationLlm::success(
            r#"{"name": "analyze-architecture", "description": "Use this skill when analyzing codebase architecture and structure.", "approach": "1. Read project structure\n2. Identify key modules\n3. Map dependencies"}"#,
        );
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();
        let record = make_record_with_tools(&["shell_exec", "file_read"]);
        let scratchpad = make_scratchpad("Analyze the codebase architecture");
        let config = make_config(true);

        consolidate_learnings(
            &memory,
            &skills,
            &llm,
            "corr-1",
            &record,
            &scratchpad,
            &config,
            None,
        )
        .await;

        let stored = skills.skills.lock().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "analyze-architecture");
        assert!(stored[0].description.contains("Use this skill when"));
        assert!(stored[0].approach.contains("Read project structure"));
    }

    #[tokio::test]
    async fn llm_returns_bad_json_falls_back_to_deterministic() {
        let llm = MockConsolidationLlm::success("This is not JSON at all!");
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();
        let record = make_record_with_tools(&["shell_exec"]);
        let scratchpad = make_scratchpad("Run the tests");
        let config = make_config(true);

        consolidate_learnings(
            &memory,
            &skills,
            &llm,
            "corr-2",
            &record,
            &scratchpad,
            &config,
            None,
        )
        .await;

        let stored = skills.skills.lock().await;
        assert_eq!(stored.len(), 1);
        // Deterministic fallback: slugified goal as name.
        assert_eq!(stored[0].name, "run-the-tests");
        assert_eq!(stored[0].description, "Run the tests");
    }

    #[tokio::test]
    async fn llm_error_falls_back_to_deterministic() {
        let llm = MockConsolidationLlm::error();
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();
        let record = make_record_with_tools(&["file_read"]);
        let scratchpad = make_scratchpad("Read the config");
        let config = make_config(true);

        consolidate_learnings(
            &memory,
            &skills,
            &llm,
            "corr-3",
            &record,
            &scratchpad,
            &config,
            None,
        )
        .await;

        let stored = skills.skills.lock().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "read-the-config");
    }

    #[tokio::test]
    async fn llm_returns_existing_skill_name_updates_fitness() {
        let llm = MockConsolidationLlm::success(
            r#"{"name": "analyze-architecture", "description": "Use this skill when analyzing architecture.", "approach": "1. Read structure\n2. Map deps"}"#,
        );
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();

        // Pre-populate with an existing skill.
        let existing = make_existing_skill("analyze-architecture");
        skills.skills.lock().await.push(existing);

        let record = make_record_with_tools(&["shell_exec", "file_read"]);
        let scratchpad = make_scratchpad("Thoroughly analyze the architecture");
        let config = make_config(true);

        consolidate_learnings(
            &memory,
            &skills,
            &llm,
            "corr-4",
            &record,
            &scratchpad,
            &config,
            None,
        )
        .await;

        let stored = skills.skills.lock().await;
        // No new skill created — just fitness updated.
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].success_count, 4); // was 3, now 4
    }

    #[tokio::test]
    async fn use_llm_extraction_disabled_uses_deterministic() {
        let llm = MockConsolidationLlm::success(
            r#"{"name": "should-not-use", "description": "nope", "approach": "nope"}"#,
        );
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();
        let record = make_record_with_tools(&["shell_exec"]);
        let scratchpad = make_scratchpad("Deploy the app");
        let config = make_config(false);

        consolidate_learnings(
            &memory,
            &skills,
            &llm,
            "corr-5",
            &record,
            &scratchpad,
            &config,
            None,
        )
        .await;

        let stored = skills.skills.lock().await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "deploy-the-app");
    }

    #[test]
    fn build_tool_trace_formats_correctly() {
        let record = make_record_with_tools(&["shell_exec", "file_read"]);
        let trace = build_tool_trace(&record);
        assert!(trace.contains("shell_exec(test input)"));
        assert!(trace.contains("file_read(test input)"));
    }

    #[test]
    fn build_tool_trace_truncates_at_limit() {
        // Create a record with many tool calls to exceed MAX_TOOL_TRACE_CHARS.
        let names: Vec<&str> = (0..200)
            .map(|_| "very_long_tool_name_for_testing")
            .collect();
        let record = make_record_with_tools(&names);
        let trace = build_tool_trace(&record);
        assert!(trace.len() <= MAX_TOOL_TRACE_CHARS + 50); // some slack for the truncation line
        assert!(trace.contains("...(truncated)"));
    }
}
