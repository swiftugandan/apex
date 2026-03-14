use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use apex_core::domain::{LogEntry, Scratchpad, SubtaskStatus};
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

    /// Analyze recent scratchpad log entries for batching opportunities.
    /// Returns an optional hint string when single-call-per-turn patterns are detected.
    fn batching_hint(log: &[LogEntry], has_pending_work: bool) -> Option<String> {
        if log.is_empty() || !has_pending_work {
            return None;
        }

        // Collect only the last 4 turns by iterating backward — avoids grouping the entire log.
        let mut recent: Vec<(u32, Vec<&str>)> = Vec::with_capacity(4);
        for entry in log.iter().rev() {
            if let Some(last) = recent.last_mut() {
                if last.0 == entry.turn {
                    last.1.push(&entry.tool_name);
                    continue;
                }
            }
            if recent.len() >= 4 {
                break;
            }
            recent.push((entry.turn, vec![&entry.tool_name]));
        }

        if recent.len() < 3 {
            return None;
        }

        // Check for repeated same-tool pattern (3 consecutive single-call turns with same tool)
        let last_3_tools: Vec<&str> = recent
            .iter()
            .take(3)
            .filter_map(|(_, tools)| {
                if tools.len() == 1 {
                    Some(tools[0])
                } else {
                    None
                }
            })
            .collect();

        if last_3_tools.len() == 3
            && last_3_tools[0] == last_3_tools[1]
            && last_3_tools[1] == last_3_tools[2]
        {
            return Some(format!(
                " You've called {} 3 turns in a row — batch them into one turn.",
                last_3_tools[0]
            ));
        }

        // Check for generic single-call streak
        let single_call_streak = recent
            .iter()
            .take_while(|(_, tools)| tools.len() == 1)
            .count();

        if single_call_streak >= 3 {
            return Some(
                " Batch: call multiple independent tools in one turn to save turns.".into(),
            );
        }

        None
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

        let (goal, subtasks, status_summary, notes, log) = {
            let pad = self.scratchpad.lock().await;
            (
                pad.goal.clone(),
                pad.subtasks.clone(),
                pad.status_summary.clone(),
                pad.notes.clone(),
                pad.log.clone(),
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

        let active: Vec<_> = subtasks
            .iter()
            .filter(|s| matches!(s.status, SubtaskStatus::Active))
            .collect();
        let pending: Vec<_> = subtasks
            .iter()
            .filter(|s| matches!(s.status, SubtaskStatus::Pending))
            .collect();

        if has_scratchpad {
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
            let _ = write!(
                out,
                " URGENT: {turns_left} turns left — call task_complete now."
            );
        } else if turn > max_turns / 2 {
            let _ = write!(out, " Past halfway — focus on output, then task_complete.");
        }

        // Context pressure warning
        if tok_pct >= 70 {
            let _ = write!(out, " Context {tok_pct}% full — be concise.");
        }

        // Batching hint: detect single-call-per-turn patterns
        let has_pending_work =
            !pending.is_empty() || !active.is_empty() || (subtasks.is_empty() && turns_left > 2);
        if let Some(hint) = Self::batching_hint(&log, has_pending_work) {
            let _ = write!(out, "{hint}");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_log_entry(turn: u32, tool: &str) -> LogEntry {
        LogEntry {
            turn,
            tool_name: tool.to_string(),
            input_summary: String::new(),
            output_summary: String::new(),
            is_error: false,
            duration_ms: 100,
        }
    }

    #[test]
    fn batching_hint_detects_single_call_streak() {
        let log = vec![
            make_log_entry(1, "shell_exec"),
            make_log_entry(2, "file_read"),
            make_log_entry(3, "grep"),
            make_log_entry(4, "shell_exec"),
        ];
        let hint = ScratchpadOrientation::batching_hint(&log, true);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("Batch"));
    }

    #[test]
    fn batching_hint_detects_same_tool_streak() {
        let log = vec![
            make_log_entry(1, "file_write"),
            make_log_entry(2, "file_write"),
            make_log_entry(3, "file_write"),
        ];
        let hint = ScratchpadOrientation::batching_hint(&log, true);
        assert!(hint.is_some());
        let hint = hint.unwrap();
        assert!(hint.contains("file_write"));
        assert!(hint.contains("3 turns in a row"));
    }

    #[test]
    fn batching_hint_silent_when_multi_call_turns() {
        let log = vec![
            make_log_entry(1, "shell_exec"),
            make_log_entry(1, "file_read"),
            make_log_entry(2, "grep"),
            make_log_entry(2, "file_write"),
            make_log_entry(3, "shell_exec"),
            make_log_entry(3, "file_read"),
        ];
        let hint = ScratchpadOrientation::batching_hint(&log, true);
        assert!(hint.is_none());
    }

    #[test]
    fn batching_hint_silent_when_no_pending_work() {
        let log = vec![
            make_log_entry(1, "shell_exec"),
            make_log_entry(2, "file_read"),
            make_log_entry(3, "grep"),
        ];
        let hint = ScratchpadOrientation::batching_hint(&log, false);
        assert!(hint.is_none());
    }

    #[test]
    fn batching_hint_silent_with_few_turns() {
        let log = vec![
            make_log_entry(1, "shell_exec"),
            make_log_entry(2, "file_read"),
        ];
        let hint = ScratchpadOrientation::batching_hint(&log, true);
        assert!(hint.is_none());
    }

    #[tokio::test]
    async fn orientation_includes_batching_hint() {
        let mut pad = Scratchpad::new("test-job", "Build a website");
        // Add 4 single-call turns
        for turn in 1..=4 {
            pad.log.push(make_log_entry(turn, "file_write"));
        }
        let provider = ScratchpadOrientation::new(Arc::new(Mutex::new(pad)));
        let result = provider.build(5, 32, 1000, 100_000, None).await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("file_write"));
        assert!(text.contains("3 turns in a row"));
    }

    #[tokio::test]
    async fn orientation_generic_batch_hint_with_different_tools() {
        let mut pad = Scratchpad::new("test-job", "Refactor codebase");
        pad.log.push(make_log_entry(1, "shell_exec"));
        pad.log.push(make_log_entry(2, "file_read"));
        pad.log.push(make_log_entry(3, "grep"));
        // No subtasks, but turns_left > 2 → has_pending_work = true
        let provider = ScratchpadOrientation::new(Arc::new(Mutex::new(pad)));
        let result = provider.build(4, 32, 1000, 100_000, None).await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(
            text.contains("Batch"),
            "Expected generic batch hint, got: {text}"
        );
    }

    #[tokio::test]
    async fn orientation_no_batch_hint_when_nearly_done() {
        let mut pad = Scratchpad::new("test-job", "Write report");
        pad.log.push(make_log_entry(1, "shell_exec"));
        pad.log.push(make_log_entry(2, "file_read"));
        pad.log.push(make_log_entry(3, "grep"));
        // turn 30/32 → turns_left = 2, no subtasks → has_pending_work = false
        let provider = ScratchpadOrientation::new(Arc::new(Mutex::new(pad)));
        let result = provider.build(30, 32, 1000, 100_000, None).await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(
            !text.contains("Batch"),
            "Should NOT batch-hint near end, got: {text}"
        );
    }

    #[tokio::test]
    async fn orientation_batch_hint_with_pending_subtasks() {
        use apex_core::domain::{SubtaskEntry, SubtaskStatus};
        let mut pad = Scratchpad::new("test-job", "Build pages");
        pad.subtasks.push(SubtaskEntry {
            index: 0,
            description: "Write page 1".into(),
            status: SubtaskStatus::Done,
            task_id: None,
            depends_on: None,
        });
        pad.subtasks.push(SubtaskEntry {
            index: 1,
            description: "Write page 2".into(),
            status: SubtaskStatus::Pending,
            task_id: None,
            depends_on: None,
        });
        pad.log.push(make_log_entry(1, "file_write"));
        pad.log.push(make_log_entry(2, "file_write"));
        pad.log.push(make_log_entry(3, "file_write"));
        let provider = ScratchpadOrientation::new(Arc::new(Mutex::new(pad)));
        // Even near end of turn budget, pending subtask → has_pending_work = true
        let result = provider.build(30, 32, 1000, 100_000, None).await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(
            text.contains("file_write"),
            "Expected same-tool hint with pending subtask, got: {text}"
        );
    }

    #[tokio::test]
    async fn orientation_no_batch_hint_when_already_batching() {
        let mut pad = Scratchpad::new("test-job", "Do stuff");
        // Turns with 2+ calls each — model is already batching
        pad.log.push(make_log_entry(1, "shell_exec"));
        pad.log.push(make_log_entry(1, "file_read"));
        pad.log.push(make_log_entry(2, "grep"));
        pad.log.push(make_log_entry(2, "file_write"));
        pad.log.push(make_log_entry(3, "shell_exec"));
        pad.log.push(make_log_entry(3, "file_read"));
        let provider = ScratchpadOrientation::new(Arc::new(Mutex::new(pad)));
        let result = provider.build(4, 32, 1000, 100_000, None).await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(
            !text.contains("Batch") && !text.contains("batch"),
            "Should NOT hint when already batching, got: {text}"
        );
    }
}
