use apex_core::config::ConsolidationSection;
use apex_core::domain::{
    slugify, AttemptOutcome, AttemptRecord, Fact, FactId, Scratchpad, Skill, SkillId,
    SkillStatus, SubtaskStatus,
};
use apex_core::ports::{HookRegistry, MemoryStore, SkillStore};

use crate::log::dispatch_log;

async fn log_consolidation_err(hooks: Option<&dyn HookRegistry>, context: &str, error: &str) {
    let fallback = format!("  consolidation: {context}: {error}");
    dispatch_log(
        hooks,
        || serde_json::json!({
            "level": "warn",
            "event": "consolidation_error",
            "context": context,
            "error": error,
        }),
        &fallback,
    )
    .await;
}

/// Best-effort post-execution learning extraction.
pub async fn consolidate_learnings(
    store: &dyn MemoryStore,
    skill_store: &dyn SkillStore,
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
                                log_consolidation_err(hooks, "failed to store fact", &e.to_string()).await;
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Skills: update fitness for successful tasks
    if config.extract_skills {
        let title = &scratchpad.goal;
        if !title.is_empty() {
            match skill_store.find_skill(title).await {
                Ok(Some(skill)) => {
                    if let Err(e) = skill_store
                        .update_skill_fitness(&skill.id, record.outcome == AttemptOutcome::Success)
                        .await
                    {
                        log_consolidation_err(hooks, "failed to update skill fitness", &e.to_string()).await;
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

                    if !tools_used.is_empty() && record.outcome == AttemptOutcome::Success {
                        let approach = record
                            .final_text
                            .as_deref()
                            .unwrap_or("")
                            .lines()
                            .take(3)
                            .collect::<Vec<_>>()
                            .join(" ");
                        let skill = Skill {
                            id: SkillId(String::new()),
                            name: slugify(title),
                            description: title.to_string(),
                            task_pattern: title.to_string(),
                            approach,
                            tools_used,
                            criteria_template: None,
                            success_count: 1,
                            failure_count: 0,
                            fitness: 0.5,
                            min_samples: 3,
                            last_used: String::new(),
                            notes: String::new(),
                            status: SkillStatus::Active,
                        };
                        if let Err(e) = skill_store.store_skill(skill).await {
                            log_consolidation_err(hooks, "failed to store skill", &e.to_string()).await;
                        }
                    }
                }
                Err(e) => {
                    log_consolidation_err(hooks, "failed to find skill", &e.to_string()).await;
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

        let pattern = format!("decompose: {}", scratchpad.goal);
        match skill_store.find_skill(&pattern).await {
            Ok(Some(skill)) => {
                let success = scratchpad
                    .subtasks
                    .iter()
                    .all(|st| st.status == SubtaskStatus::Done);
                if let Err(e) = skill_store.update_skill_fitness(&skill.id, success).await {
                    log_consolidation_err(hooks, "failed to update decomposition skill fitness", &e.to_string()).await;
                }
            }
            Ok(None) => {
                let skill = Skill {
                    id: SkillId(String::new()),
                    name: slugify(&format!("decompose-{}", scratchpad.goal)),
                    description: format!("Decomposition strategy for: {}", scratchpad.goal),
                    task_pattern: pattern,
                    approach: decomposition,
                    tools_used: vec![],
                    criteria_template: None,
                    success_count: if record.outcome == AttemptOutcome::Success { 1 } else { 0 },
                    failure_count: if record.outcome == AttemptOutcome::Failed { 1 } else { 0 },
                    fitness: 0.5,
                    min_samples: 3,
                    last_used: String::new(),
                    notes: format!("avg_subtasks: {}", scratchpad.subtasks.len()),
                    status: SkillStatus::Active,
                };
                if let Err(e) = skill_store.store_skill(skill).await {
                    log_consolidation_err(hooks, "failed to store decomposition skill", &e.to_string()).await;
                }
            }
            Err(e) => {
                log_consolidation_err(hooks, "failed to find decomposition skill", &e.to_string()).await;
            }
        }
    }
}
