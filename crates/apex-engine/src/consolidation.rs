use apex_core::config::ConsolidationSection;
use apex_core::domain::{
    slugify, AttemptOutcome, AttemptRecord, Fact, FactId, Scratchpad, Skill, SkillId, SkillStatus,
    SubtaskStatus,
};
use apex_core::ports::{HookRegistry, MemoryStore, SkillExtractor, SkillStore};

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

/// Best-effort post-execution learning extraction.
#[allow(clippy::too_many_arguments)]
pub async fn consolidate_learnings(
    store: &dyn MemoryStore,
    skill_store: &dyn SkillStore,
    skill_extractor: Option<&dyn SkillExtractor>,
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
            // Try LLM-powered extraction first (if extractor provided), fall back to deterministic.
            let extracted = if let Some(extractor) = skill_extractor {
                extractor.extract_skill(title, record, skill_store).await
            } else {
                None
            };

            let (skill_name, skill_description, skill_approach) = match extracted {
                Some(es) => (es.name, es.description, es.approach),
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
        AttemptOutcome, AttemptRecord, ExtractedSkill, Skill, SkillId, SkillStatus, TokenUsage,
        ToolCallRecord, TurnRecord,
    };

    use crate::test_mocks::{MockMemoryStore, MockSkillExtractor, MockSkillStore};

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

    fn make_config() -> ConsolidationSection {
        ConsolidationSection::default()
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
    async fn extractor_returns_some_extracts_skill() {
        let extractor = MockSkillExtractor::new(Some(ExtractedSkill {
            name: "analyze-architecture".to_string(),
            description: "Use this skill when analyzing codebase architecture and structure."
                .to_string(),
            approach: "1. Read project structure\n2. Identify key modules\n3. Map dependencies"
                .to_string(),
        }));
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();
        let record = make_record_with_tools(&["shell_exec", "file_read"]);
        let scratchpad = make_scratchpad("Analyze the codebase architecture");
        let config = make_config();

        consolidate_learnings(
            &memory,
            &skills,
            Some(&extractor),
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
    async fn extractor_returns_none_falls_back_to_deterministic() {
        let extractor = MockSkillExtractor::new(None);
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();
        let record = make_record_with_tools(&["shell_exec"]);
        let scratchpad = make_scratchpad("Run the tests");
        let config = make_config();

        consolidate_learnings(
            &memory,
            &skills,
            Some(&extractor),
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
    async fn no_extractor_uses_deterministic() {
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();
        let record = make_record_with_tools(&["file_read"]);
        let scratchpad = make_scratchpad("Read the config");
        let config = make_config();

        consolidate_learnings(
            &memory,
            &skills,
            None,
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
    async fn extractor_returns_existing_skill_name_updates_fitness() {
        let extractor = MockSkillExtractor::new(Some(ExtractedSkill {
            name: "analyze-architecture".to_string(),
            description: "Use this skill when analyzing architecture.".to_string(),
            approach: "1. Read structure\n2. Map deps".to_string(),
        }));
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();

        // Pre-populate with an existing skill.
        let existing = make_existing_skill("analyze-architecture");
        skills.skills.lock().await.push(existing);

        let record = make_record_with_tools(&["shell_exec", "file_read"]);
        let scratchpad = make_scratchpad("Thoroughly analyze the architecture");
        let config = make_config();

        consolidate_learnings(
            &memory,
            &skills,
            Some(&extractor),
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
    async fn skill_extractor_none_uses_deterministic() {
        let memory = MockMemoryStore::new();
        let skills = MockSkillStore::new();
        let record = make_record_with_tools(&["shell_exec"]);
        let scratchpad = make_scratchpad("Deploy the app");
        let config = make_config();

        consolidate_learnings(
            &memory,
            &skills,
            None,
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
}
