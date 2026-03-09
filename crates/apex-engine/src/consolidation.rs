use apex_core::domain::{
    AttemptOutcome, AttemptRecord, Fact, FactId, Scratchpad, Skill, SkillId,
    Strategy, StrategyId, SubtaskStatus,
};
use apex_core::ports::MemoryStore;

/// Best-effort post-execution learning extraction.
pub async fn consolidate_learnings(
    store: &dyn MemoryStore,
    correlation_id: &str,
    record: &AttemptRecord,
    scratchpad: &Scratchpad,
) {
    // 1. Extract facts from "## New Facts Discovered" sections
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
                            eprintln!("  consolidation: failed to store fact: {e}");
                        }
                    }
                }
            }
        }
    }

    // 2. Skills: update fitness for successful tasks
    let title = &scratchpad.goal;
    if !title.is_empty() {
        match store.find_skill(title).await {
            Ok(Some(skill)) => {
                if let Err(e) = store
                    .update_skill_fitness(&skill.id, record.outcome == AttemptOutcome::Success)
                    .await
                {
                    eprintln!("  consolidation: failed to update skill fitness: {e}");
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
                    let skill = Skill {
                        id: SkillId(String::new()),
                        task_pattern: title.to_string(),
                        approach: record
                            .final_text
                            .as_deref()
                            .unwrap_or("")
                            .lines()
                            .take(3)
                            .collect::<Vec<_>>()
                            .join(" "),
                        tools_used,
                        criteria_template: None,
                        success_count: 1,
                        failure_count: 0,
                        fitness: 0.5,
                        min_samples: 3,
                        last_used: String::new(),
                        notes: String::new(),
                    };
                    if let Err(e) = store.store_skill(skill).await {
                        eprintln!("  consolidation: failed to store skill: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("  consolidation: failed to find skill: {e}");
            }
        }
    }

    // 3. Strategies: for jobs with subtasks
    if !scratchpad.subtasks.is_empty() && !scratchpad.goal.is_empty() {
        let decomposition = scratchpad
            .subtasks
            .iter()
            .map(|st| format!("{}. {}", st.index, st.description))
            .collect::<Vec<_>>()
            .join("\n");

        match store.find_strategy(&scratchpad.goal).await {
            Ok(Some(strategy)) => {
                let success = scratchpad
                    .subtasks
                    .iter()
                    .all(|st| st.status == SubtaskStatus::Done);
                if let Err(e) = store.update_strategy_fitness(&strategy.id, success).await {
                    eprintln!("  consolidation: failed to update strategy fitness: {e}");
                }
            }
            Ok(None) => {
                let strategy = Strategy {
                    id: StrategyId(String::new()),
                    goal_pattern: scratchpad.goal.clone(),
                    decomposition,
                    avg_subtasks: scratchpad.subtasks.len() as f64,
                    avg_duration_secs: 0.0,
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
                    notes: String::new(),
                };
                if let Err(e) = store.store_strategy(strategy).await {
                    eprintln!("  consolidation: failed to store strategy: {e}");
                }
            }
            Err(e) => {
                eprintln!("  consolidation: failed to find strategy: {e}");
            }
        }
    }
}
