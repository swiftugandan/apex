use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use apex_core::domain::{Scratchpad, SubtaskStatus};
use apex_core::ports::OrientationProvider;
use apex_engine::OrientationFactory;

/// Scratchpad-sourced orientation provider. Reads goal, subtasks, notes, and
/// status from the shared scratchpad each turn and renders a compact directive
/// block for injection into the conversation.
pub struct ScratchpadOrientation {
    scratchpad: Arc<Mutex<Scratchpad>>,
}

impl ScratchpadOrientation {
    pub fn new(scratchpad: Arc<Mutex<Scratchpad>>) -> Self {
        Self { scratchpad }
    }
}

#[async_trait]
impl OrientationProvider for ScratchpadOrientation {
    async fn build(
        &self,
        turn: usize,
        max_turns: usize,
        estimated_tokens: u32,
        context_window: usize,
        compaction_info: Option<(usize, usize)>,
    ) -> Option<String> {
        // Turn 1: the task body already provides full context — skip.
        if turn <= 1 {
            return None;
        }

        let (goal, subtasks, status_summary, notes) = {
            let pad = self.scratchpad.lock().await;
            (
                pad.goal.clone(),
                pad.subtasks.clone(),
                pad.status_summary.clone(),
                pad.notes.clone(),
            )
        };

        let has_scratchpad = !goal.is_empty();
        if !has_scratchpad && compaction_info.is_none() {
            return None;
        }

        let tok_pct = if context_window > 0 {
            (estimated_tokens as usize * 100) / context_window
        } else {
            0
        };
        let turns_left = max_turns.saturating_sub(turn);
        let mut out = format!("[turn {turn}/{max_turns}, {tok_pct}% context]");

        if has_scratchpad {
            // Show active subtask(s) as the immediate directive
            let active: Vec<_> = subtasks
                .iter()
                .filter(|s| matches!(s.status, SubtaskStatus::Active))
                .collect();
            let pending: Vec<_> = subtasks
                .iter()
                .filter(|s| matches!(s.status, SubtaskStatus::Pending))
                .collect();
            let done_count = subtasks
                .iter()
                .filter(|s| matches!(s.status, SubtaskStatus::Done))
                .count();

            if !subtasks.is_empty() {
                let total = subtasks.len();
                let _ = write!(out, " {done_count}/{total} done.");

                if !active.is_empty() {
                    let desc = apex_core::truncate_str(&active[0].description, 80);
                    let _ = write!(out, " NOW: {desc}");
                    for a in active.iter().skip(1).take(2) {
                        let desc = apex_core::truncate_str(&a.description, 60);
                        let _ = write!(out, " + {desc}");
                    }
                } else if !pending.is_empty() {
                    let desc = apex_core::truncate_str(&pending[0].description, 80);
                    let _ = write!(out, " NEXT: {desc}");
                }
            } else if !status_summary.is_empty() {
                let status = apex_core::truncate_str(&status_summary, 80);
                let _ = write!(out, " Status: {status}");
            }

            // Last note only — most recent context
            if let Some(note) = notes.last() {
                let note = apex_core::truncate_str(note, 80);
                let _ = write!(out, " Note: {note}");
            }
        }

        if let Some((count, at_turn)) = compaction_info {
            let _ = write!(out, " (compacted {count} msgs at turn {at_turn})");
        }

        // Urgency nudge when past halfway or running low on turns
        if turns_left <= 3 {
            let _ = write!(out, " URGENT: {turns_left} turns left — finish now.");
        } else if turn > max_turns / 2 {
            let _ = write!(out, " Past halfway — focus on producing output.");
        }

        // Context pressure warning
        if tok_pct >= 70 {
            let _ = write!(out, " Context {tok_pct}% full — be concise.");
        }

        // Hard cap safety net (~400 tokens ≈ 1600 chars)
        Some(apex_core::truncate_str(&out, 1600).to_string())
    }
}

/// Factory that creates `ScratchpadOrientation` providers per-claim.
pub struct ScratchpadOrientationFactory;

impl OrientationFactory for ScratchpadOrientationFactory {
    fn build(&self, scratchpad: Arc<Mutex<Scratchpad>>) -> Arc<dyn OrientationProvider> {
        Arc::new(ScratchpadOrientation::new(scratchpad))
    }
}
