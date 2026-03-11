use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use apex_core::context::MessageComposer;
use apex_core::domain::{
    HookEvent, HookOutcome, MessageHeaders, MessageType, QueueMessage, ToolCall, ToolDef,
    ToolResult, ToolSchema,
};
use apex_core::error::ToolError;
use apex_core::ports::{HookRegistry, MemoryStore, Queue, SkillStore, ToolRegistry};

pub struct QueueToolRegistry {
    queue: Arc<dyn Queue>,
    correlation_id: String,
    current_depth: u32,
    max_depth: u32,
    parent_goal: String,
    parent_body: String,
    store: Option<Arc<dyn MemoryStore>>,
    skill_store: Option<Arc<dyn SkillStore>>,
    composer: MessageComposer,
    hooks: Option<Arc<dyn HookRegistry>>,
}

impl QueueToolRegistry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        queue: Arc<dyn Queue>,
        correlation_id: String,
        current_depth: u32,
        max_depth: u32,
        parent_goal: String,
        parent_body: String,
        store: Option<Arc<dyn MemoryStore>>,
        skill_store: Option<Arc<dyn SkillStore>>,
        composer: MessageComposer,
    ) -> Self {
        Self {
            queue,
            correlation_id,
            current_depth,
            max_depth,
            parent_goal,
            parent_body,
            store,
            skill_store,
            composer,
            hooks: None,
        }
    }

    /// Set the hook registry for dispatching `before_push` events.
    pub fn with_hooks(mut self, hooks: Option<Arc<dyn HookRegistry>>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Dispatch `before_push` hooks. Returns `Err` if any hook blocks the push.
    async fn dispatch_before_push(&self, msg: &QueueMessage) -> Result<(), ToolError> {
        let Some(ref hooks) = self.hooks else {
            return Ok(());
        };
        let context = json!({
            "correlation_id": msg.headers.correlation_id,
            "message_type": format!("{:?}", msg.headers.message_type),
            "depth": msg.headers.depth,
        });
        let outcomes = hooks.dispatch(HookEvent::BeforePush, &context).await;
        for outcome in outcomes {
            if let HookOutcome::Block(reason) = outcome {
                return Err(ToolError::Execution(format!(
                    "Push blocked by hook: {reason}"
                )));
            }
        }
        Ok(())
    }

    async fn handle_decompose_goal(&self, input: &Value) -> Result<Value, ToolError> {
        if self.current_depth >= self.max_depth {
            return Err(ToolError::Execution(format!(
                "Max decomposition depth ({}) reached. Handle this task directly instead of decomposing further.",
                self.max_depth
            )));
        }

        let subtasks = input
            .get("subtasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::InvalidInput("missing 'subtasks' array".to_string()))?;

        if subtasks.is_empty() {
            return Err(ToolError::InvalidInput(
                "subtasks array must not be empty".to_string(),
            ));
        }

        let composer = &self.composer;

        struct SubtaskInfo {
            description: String,
            acceptance_criteria: String,
            depends_on_indices: Vec<usize>,
        }

        let mut infos = Vec::new();
        for (idx, subtask) in subtasks.iter().enumerate() {
            let description = subtask
                .get("description")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput(format!("subtask[{idx}] missing 'description'"))
                })?
                .to_string();

            let acceptance_criteria = subtask
                .get("acceptance_criteria")
                .and_then(|v| v.as_str())
                .unwrap_or("(to be determined by agent)")
                .to_string();

            let depends_on_indices: Vec<usize> = subtask
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect()
                })
                .unwrap_or_default();

            for &dep_idx in &depends_on_indices {
                if dep_idx >= subtasks.len() {
                    return Err(ToolError::InvalidInput(format!(
                        "subtask[{idx}] depends_on index {dep_idx} is out of range"
                    )));
                }
                if dep_idx == idx {
                    return Err(ToolError::InvalidInput(format!(
                        "subtask[{idx}] cannot depend on itself"
                    )));
                }
            }

            infos.push(SubtaskInfo {
                description,
                acceptance_criteria,
                depends_on_indices,
            });
        }

        // Detect transitive cycles via topological sort (Kahn's algorithm)
        {
            let n = infos.len();
            let mut in_degree = vec![0u32; n];
            let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
            for (i, info) in infos.iter().enumerate() {
                for &dep in &info.depends_on_indices {
                    adj[dep].push(i);
                    in_degree[i] += 1;
                }
            }
            let mut queue: std::collections::VecDeque<usize> = in_degree
                .iter()
                .enumerate()
                .filter(|(_, &d)| d == 0)
                .map(|(i, _)| i)
                .collect();
            let mut visited = 0usize;
            while let Some(node) = queue.pop_front() {
                visited += 1;
                for &next in &adj[node] {
                    in_degree[next] -= 1;
                    if in_degree[next] == 0 {
                        queue.push_back(next);
                    }
                }
            }
            if visited != n {
                return Err(ToolError::InvalidInput(
                    "subtask dependencies contain a cycle".to_string(),
                ));
            }
        }

        let mut subtask_ids: Vec<String> = Vec::new();

        for info in &infos {
            let title = info.description.lines().next().unwrap_or(&info.description);
            let title = apex_core::truncate_str(title, 80);

            let (facts, skill) = {
                let facts_fut = async {
                    if let Some(ref store) = self.store {
                        store
                            .query_facts(&info.description, 3)
                            .await
                            .ok()
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    }
                };
                let skill_fut = async {
                    if let Some(ref skill_store) = self.skill_store {
                        skill_store
                            .find_skill(&info.description)
                            .await
                            .ok()
                            .flatten()
                    } else {
                        None
                    }
                };
                tokio::join!(facts_fut, skill_fut)
            };

            let body = if !facts.is_empty() || skill.is_some() {
                composer.compose_subtask_with_memory(
                    title,
                    &info.description,
                    &info.acceptance_criteria,
                    &self.parent_goal,
                    &self.parent_body,
                    &facts,
                    skill.as_ref(),
                )
            } else {
                composer.compose_subtask(
                    title,
                    &info.description,
                    &info.acceptance_criteria,
                    &self.parent_goal,
                    &self.parent_body,
                )
            };

            let depends_on: Vec<String> = info
                .depends_on_indices
                .iter()
                .map(|&idx| subtask_ids[idx].clone())
                .collect();

            let msg = QueueMessage {
                headers: MessageHeaders {
                    message_type: MessageType::Subtask,
                    correlation_id: self.correlation_id.clone(),
                    depth: self.current_depth + 1,
                    retry_count: 0,
                    depends_on,
                },
                body,
            };

            self.dispatch_before_push(&msg).await?;

            let id = self
                .queue
                .push(msg)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            subtask_ids.push(id);
        }

        let continuation_body = MessageComposer::compose_continuation(
            &self.correlation_id,
            &self.parent_goal,
            &subtask_ids,
        );

        let continuation_msg = QueueMessage {
            headers: MessageHeaders {
                message_type: MessageType::Continuation,
                correlation_id: self.correlation_id.clone(),
                depth: self.current_depth,
                retry_count: 0,
                depends_on: subtask_ids.clone(),
            },
            body: continuation_body,
        };

        self.dispatch_before_push(&continuation_msg).await?;

        let continuation_id = self
            .queue
            .push(continuation_msg)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(json!({
            "status": "decomposed",
            "subtask_ids": subtask_ids,
            "continuation_id": continuation_id,
            "message": format!("Created {} subtask(s) and 1 continuation message", subtask_ids.len())
        }))
    }

    async fn handle_queue_read_done(&self, input: &Value) -> Result<Value, ToolError> {
        let correlation_id = input
            .get("correlation_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.correlation_id);

        let done_ids = self
            .queue
            .list_done(correlation_id)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let mut results = Vec::new();
        for id in &done_ids {
            let body = self
                .queue
                .read_done_body(id)
                .await
                .map_err(|e| ToolError::Execution(e.to_string()))?;

            results.push(json!({
                "id": id,
                "body": body,
            }));
        }

        Ok(json!({
            "correlation_id": correlation_id,
            "count": results.len(),
            "results": results,
        }))
    }

    /// Check dependency graph for cycles using topological sort.
    /// Returns Ok(()) if DAG is valid, Err if cycle detected.
    #[cfg(test)]
    fn check_cycle(deps: &[Vec<usize>]) -> Result<(), &'static str> {
        let n = deps.len();
        let mut in_degree = vec![0u32; n];
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, dep_list) in deps.iter().enumerate() {
            for &dep in dep_list {
                adj[dep].push(i);
                in_degree[i] += 1;
            }
        }
        let mut queue: std::collections::VecDeque<usize> = in_degree
            .iter()
            .enumerate()
            .filter(|(_, &d)| d == 0)
            .map(|(i, _)| i)
            .collect();
        let mut visited = 0usize;
        while let Some(node) = queue.pop_front() {
            visited += 1;
            for &next in &adj[node] {
                in_degree[next] -= 1;
                if in_degree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }
        if visited == n {
            Ok(())
        } else {
            Err("cycle")
        }
    }
}

#[async_trait]
impl ToolRegistry for QueueToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                schema: ToolSchema {
                    name: "decompose_goal".to_string(),
                    description: "Decompose a complex goal into subtasks that will be executed independently and in parallel where possible. Use this when a task has 2 or more independent steps. Each subtask becomes a separate queue message processed by an agent instance. Write acceptance_criteria in plain natural language describing what 'done' looks like — an LLM judge with tool access will verify completion.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "subtasks": {
                                "type": "array",
                                "description": "List of subtasks to create",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "description": {
                                            "type": "string",
                                            "description": "What this subtask should accomplish"
                                        },
                                        "acceptance_criteria": {
                                            "type": "string",
                                            "description": "Plain natural language describing what 'done' looks like. An LLM judge with tool access (shell_exec, file_read) will independently verify. Example: 'The file /tmp/out.txt exists and contains hello. Running `cargo test` passes with no failures.'"
                                        },
                                        "depends_on": {
                                            "type": "array",
                                            "description": "Indices (0-based) of subtasks this depends on",
                                            "items": { "type": "integer" }
                                        }
                                    },
                                    "required": ["description"]
                                }
                            }
                        },
                        "required": ["subtasks"]
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "queue_read_done".to_string(),
                    description: "Read completed subtask results from the queue. Use this in continuation messages to collect results from all completed subtasks before assembling the final deliverable.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "correlation_id": {
                                "type": "string",
                                "description": "The correlation ID to filter by. Defaults to the current job's correlation ID."
                            }
                        }
                    }),
                },
            },
        ]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let result = match call.name.as_str() {
            "decompose_goal" => self.handle_decompose_goal(&call.input).await?,
            "queue_read_done" => self.handle_queue_read_done(&call.input).await?,
            _ => return Err(ToolError::UnknownTool(call.name.clone())),
        };

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: result,
            is_error: false,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests for cycle detection logic (same algorithm used in handle_decompose_goal)

    #[test]
    fn decompose_rejects_mutual_dependency() {
        // A depends on B, B depends on A
        let deps = vec![
            vec![1], // 0 depends on 1
            vec![0], // 1 depends on 0
        ];
        assert!(QueueToolRegistry::check_cycle(&deps).is_err());
    }

    #[test]
    fn decompose_rejects_chain_cycle() {
        // A→B→C→A
        let deps = vec![
            vec![2], // 0 depends on 2
            vec![0], // 1 depends on 0
            vec![1], // 2 depends on 1
        ];
        assert!(QueueToolRegistry::check_cycle(&deps).is_err());
    }

    #[test]
    fn decompose_allows_valid_dag() {
        // Linear chain: 0 → 1 → 2, plus 3 depends on both 1 and 2 (fan-in)
        let deps = vec![
            vec![],     // 0 has no deps
            vec![0],    // 1 depends on 0
            vec![1],    // 2 depends on 1
            vec![1, 2], // 3 depends on 1 and 2
        ];
        assert!(QueueToolRegistry::check_cycle(&deps).is_ok());
    }

    #[test]
    fn decompose_allows_independent_tasks() {
        // No dependencies at all
        let deps = vec![vec![], vec![], vec![]];
        assert!(QueueToolRegistry::check_cycle(&deps).is_ok());
    }

    #[test]
    fn decompose_rejects_longer_cycle_in_subgraph() {
        // 0 is independent, 1→2→3→1 form a cycle
        let deps = vec![
            vec![],  // 0: independent
            vec![3], // 1 depends on 3
            vec![1], // 2 depends on 1
            vec![2], // 3 depends on 2
        ];
        assert!(QueueToolRegistry::check_cycle(&deps).is_err());
    }

    // Integration test via handle_decompose_goal (requires a MockQueue)
    #[tokio::test]
    async fn decompose_goal_rejects_mutual_cycle() {
        use apex_core::context::MessageComposer;
        use std::sync::Arc;

        // Minimal mock queue
        struct MinimalQueue;
        #[async_trait]
        impl Queue for MinimalQueue {
            async fn push(
                &self,
                _msg: QueueMessage,
            ) -> Result<String, apex_core::error::QueueError> {
                Ok("id".into())
            }
            async fn pop(
                &self,
            ) -> Result<Option<apex_core::domain::ClaimedTask>, apex_core::error::QueueError>
            {
                Ok(None)
            }
            async fn update_body(
                &self,
                _c: &apex_core::domain::ClaimedTask,
                _b: &str,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn ack(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn nack(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn nack_with_delay(
                &self,
                _c: &apex_core::domain::ClaimedTask,
                _d: std::time::Duration,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn reject(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn depth(
                &self,
            ) -> Result<apex_core::domain::QueueDepth, apex_core::error::QueueError> {
                Ok(Default::default())
            }
            async fn reap(
                &self,
            ) -> Result<apex_core::domain::ReapResult, apex_core::error::QueueError> {
                Ok(Default::default())
            }
            async fn list_done(
                &self,
                _cid: &str,
            ) -> Result<Vec<String>, apex_core::error::QueueError> {
                Ok(vec![])
            }
            async fn read_done_body(
                &self,
                _id: &str,
            ) -> Result<String, apex_core::error::QueueError> {
                Ok(String::new())
            }
            async fn list_with_state(
                &self,
                _s: &str,
            ) -> Result<Vec<apex_core::domain::QueueMessageMeta>, apex_core::error::QueueError>
            {
                Ok(vec![])
            }
        }

        let registry = QueueToolRegistry::new(
            Arc::new(MinimalQueue),
            "corr-1".into(),
            0, // current_depth
            3, // max_depth
            "parent goal".into(),
            "parent body".into(),
            None,
            None,
            MessageComposer::default(),
        );

        let input = json!({
            "subtasks": [
                { "description": "Task A", "depends_on": [1] },
                { "description": "Task B", "depends_on": [0] }
            ]
        });

        let result = registry.handle_decompose_goal(&input).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(ref msg) if msg.contains("cycle")),
            "expected cycle error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn before_push_hook_blocks_decompose() {
        use apex_core::context::MessageComposer;
        use apex_core::domain::{HookDef, HookOutcome};
        use apex_core::ports::HookRegistry;
        use std::sync::Arc;

        /// A mock hook registry that always blocks before_push events.
        struct BlockingHookRegistry;

        #[async_trait]
        impl HookRegistry for BlockingHookRegistry {
            fn hooks_for(&self, _event: HookEvent) -> Vec<HookDef> {
                vec![]
            }
            fn all_hooks(&self) -> Vec<HookDef> {
                vec![]
            }
            async fn dispatch(
                &self,
                event: HookEvent,
                _context: &serde_json::Value,
            ) -> Vec<HookOutcome> {
                if event == HookEvent::BeforePush {
                    vec![HookOutcome::Block("Push blocked by test".into())]
                } else {
                    vec![]
                }
            }
            fn reload(&mut self) -> Result<(), String> {
                Ok(())
            }
        }

        struct MinimalQueue2;
        #[async_trait]
        impl Queue for MinimalQueue2 {
            async fn push(
                &self,
                _msg: QueueMessage,
            ) -> Result<String, apex_core::error::QueueError> {
                Ok("id".into())
            }
            async fn pop(
                &self,
            ) -> Result<Option<apex_core::domain::ClaimedTask>, apex_core::error::QueueError>
            {
                Ok(None)
            }
            async fn update_body(
                &self,
                _c: &apex_core::domain::ClaimedTask,
                _b: &str,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn ack(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn nack(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn nack_with_delay(
                &self,
                _c: &apex_core::domain::ClaimedTask,
                _d: std::time::Duration,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn reject(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn depth(
                &self,
            ) -> Result<apex_core::domain::QueueDepth, apex_core::error::QueueError> {
                Ok(Default::default())
            }
            async fn reap(
                &self,
            ) -> Result<apex_core::domain::ReapResult, apex_core::error::QueueError> {
                Ok(Default::default())
            }
            async fn list_done(
                &self,
                _cid: &str,
            ) -> Result<Vec<String>, apex_core::error::QueueError> {
                Ok(vec![])
            }
            async fn read_done_body(
                &self,
                _id: &str,
            ) -> Result<String, apex_core::error::QueueError> {
                Ok(String::new())
            }
            async fn list_with_state(
                &self,
                _s: &str,
            ) -> Result<Vec<apex_core::domain::QueueMessageMeta>, apex_core::error::QueueError>
            {
                Ok(vec![])
            }
        }

        let hooks: Arc<dyn HookRegistry> = Arc::new(BlockingHookRegistry);
        let registry = QueueToolRegistry::new(
            Arc::new(MinimalQueue2),
            "corr-1".into(),
            0,
            3,
            "parent goal".into(),
            "parent body".into(),
            None,
            None,
            MessageComposer::default(),
        )
        .with_hooks(Some(hooks));

        let input = json!({
            "subtasks": [
                { "description": "Task A" },
            ]
        });

        let result = registry.handle_decompose_goal(&input).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref msg) if msg.contains("blocked")),
            "expected block error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn no_hooks_allows_push() {
        use apex_core::context::MessageComposer;
        use std::sync::Arc;

        struct MinimalQueue3;
        #[async_trait]
        impl Queue for MinimalQueue3 {
            async fn push(
                &self,
                _msg: QueueMessage,
            ) -> Result<String, apex_core::error::QueueError> {
                Ok("id".into())
            }
            async fn pop(
                &self,
            ) -> Result<Option<apex_core::domain::ClaimedTask>, apex_core::error::QueueError>
            {
                Ok(None)
            }
            async fn update_body(
                &self,
                _c: &apex_core::domain::ClaimedTask,
                _b: &str,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn ack(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn nack(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn nack_with_delay(
                &self,
                _c: &apex_core::domain::ClaimedTask,
                _d: std::time::Duration,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn reject(
                &self,
                _c: &apex_core::domain::ClaimedTask,
            ) -> Result<(), apex_core::error::QueueError> {
                Ok(())
            }
            async fn depth(
                &self,
            ) -> Result<apex_core::domain::QueueDepth, apex_core::error::QueueError> {
                Ok(Default::default())
            }
            async fn reap(
                &self,
            ) -> Result<apex_core::domain::ReapResult, apex_core::error::QueueError> {
                Ok(Default::default())
            }
            async fn list_done(
                &self,
                _cid: &str,
            ) -> Result<Vec<String>, apex_core::error::QueueError> {
                Ok(vec![])
            }
            async fn read_done_body(
                &self,
                _id: &str,
            ) -> Result<String, apex_core::error::QueueError> {
                Ok(String::new())
            }
            async fn list_with_state(
                &self,
                _s: &str,
            ) -> Result<Vec<apex_core::domain::QueueMessageMeta>, apex_core::error::QueueError>
            {
                Ok(vec![])
            }
        }

        // No hooks — push should succeed
        let registry = QueueToolRegistry::new(
            Arc::new(MinimalQueue3),
            "corr-1".into(),
            0,
            3,
            "parent goal".into(),
            "parent body".into(),
            None,
            None,
            MessageComposer::default(),
        );

        let input = json!({
            "subtasks": [
                { "description": "Task A" },
            ]
        });

        let result = registry.handle_decompose_goal(&input).await;
        assert!(result.is_ok());
    }
}
