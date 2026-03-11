use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use async_trait::async_trait;

use apex_core::config::{CompactionSection, ConsolidationSection, Invariants, MemoryMode, RoleProfile};
use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::domain::{MessageHeaders, MessageType, QueueMessage};
use apex_core::error::ToolError;
use apex_core::ports::{HookRegistry, LlmProvider, MemoryStore, Queue, SkillStore, ToolRegistry, WorkingMemory};

use apex_core::ports::{SubAgentResult, SubAgentSpawner};

use apex_tools::FilteredHookRegistry;

use crate::paths::ProjectPaths;
use crate::registry::{build_static_tools, CompositeToolRegistry, OwnedFilteredToolRegistry};
use crate::worker::{worker_loop, WorkerContext, WorkerLimits};

/// Factory closures for creating infra components without depending on apex-infra.
pub struct InfraFactories {
    pub queue: Arc<dyn Fn(&std::path::Path) -> Result<Arc<dyn Queue>, String> + Send + Sync>,
    pub working_memory: Arc<dyn Fn(&std::path::Path) -> Arc<dyn WorkingMemory> + Send + Sync>,
    pub memory_store: Arc<dyn Fn(&std::path::Path) -> Result<Arc<dyn MemoryStore>, String> + Send + Sync>,
    pub skill_store: Arc<dyn Fn(&std::path::Path) -> Arc<dyn SkillStore> + Send + Sync>,
}

/// Static configuration for sub-agent spawning.
pub struct SpawnerConfig {
    pub invariants: Arc<Invariants>,
    pub roles: Arc<[RoleProfile]>,
    pub max_tool_result_bytes: usize,
    pub max_output_tokens: u32,
    pub remaining_delegate_depth: u32,
    pub max_turns: usize,
    pub max_empty_cycles: u32,
    pub compaction: CompactionSection,
    pub consolidation: ConsolidationSection,
}

/// Concrete SubAgentSpawner that runs sub-agents in-process with their own
/// queue, working memory, and (optionally isolated) long-term memory.
pub struct InProcessSpawner {
    pub project_paths: ProjectPaths,
    pub parent_long_term: Arc<dyn MemoryStore>,
    pub parent_skills: Arc<dyn SkillStore>,
    pub llm: Arc<dyn LlmProvider>,
    pub estimator: Arc<Mutex<TokenEstimator>>,
    pub config: SpawnerConfig,
    pub infra: Arc<InfraFactories>,
    /// Optional hooks from the parent agent. Only hooks with `propagate: true` are inherited.
    pub hooks: Option<Arc<dyn HookRegistry>>,
}

#[async_trait]
impl SubAgentSpawner for InProcessSpawner {
    async fn spawn(
        &self,
        task: &str,
        role: &RoleProfile,
        persona: &str,
    ) -> Result<SubAgentResult, ToolError> {
        let short_uuid = apex_core::generate_id(&role.name);

        // 1. Create ephemeral queue
        let sub_queue_dir = self.project_paths.queues_dir.join(format!("sub-{}", short_uuid));
        let sub_queue = (self.infra.queue)(&sub_queue_dir)
            .map_err(|e| ToolError::Execution(format!("failed to create sub-agent queue: {e}")))?;

        // 2. Create ephemeral working memory directory
        let sub_memory_dir = self.project_paths.working_memory.join(format!("sub-{}", short_uuid));
        tokio::fs::create_dir_all(&sub_memory_dir).await
            .map_err(|e| ToolError::Execution(format!("failed to create sub-agent memory dir: {e}")))?;
        let sub_memory = (self.infra.working_memory)(&sub_memory_dir);

        // 3. Resolve long-term memory based on role's memory mode
        let sub_long_term: Arc<dyn MemoryStore> = match role.memory {
            MemoryMode::Shared => Arc::clone(&self.parent_long_term),
            MemoryMode::Isolated => {
                let isolated_dir = self.project_paths.long_term_dir.join(format!("sub-{}", short_uuid));
                let db_path = isolated_dir.join("memory.db");
                (self.infra.memory_store)(&db_path)
                    .map_err(|e| ToolError::Execution(format!("failed to create isolated memory: {e}")))?
            }
        };

        // 3b. Skills are always shared (they're files on disk)
        let sub_skills: Arc<dyn SkillStore> = Arc::clone(&self.parent_skills);

        // 4. Resolve LLM — use role's model override if different
        // For now, we always use the parent's LLM since creating a new provider
        // requires infra knowledge. The model override would need the factory pattern too.
        let sub_llm = Arc::clone(&self.llm);

        // 5. Build a new spawner for sub-sub-agents (with decremented depth)
        let sub_depth = if role.can_delegate {
            self.config.remaining_delegate_depth.saturating_sub(1)
        } else {
            0
        };
        // Propagate only hooks marked with `propagate: true`
        let sub_hooks: Option<Arc<dyn HookRegistry>> = self
            .hooks
            .as_ref()
            .and_then(|h| FilteredHookRegistry::from_propagatable(h.as_ref()));

        let sub_spawner: Arc<dyn SubAgentSpawner> = Arc::new(InProcessSpawner {
            project_paths: self.project_paths.clone(),
            parent_long_term: sub_long_term.clone(),
            parent_skills: sub_skills.clone(),
            llm: sub_llm.clone(),
            estimator: Arc::clone(&self.estimator),
            config: SpawnerConfig {
                invariants: Arc::clone(&self.config.invariants),
                roles: Arc::clone(&self.config.roles),
                max_tool_result_bytes: self.config.max_tool_result_bytes,
                max_output_tokens: self.config.max_output_tokens,
                remaining_delegate_depth: sub_depth,
                max_turns: self.config.max_turns,
                max_empty_cycles: self.config.max_empty_cycles,
                compaction: self.config.compaction.clone(),
                consolidation: self.config.consolidation.clone(),
            },
            infra: Arc::clone(&self.infra),
            hooks: sub_hooks.clone(),
        });

        // 6. Build static tools for sub-agent
        let sub_static_tools = build_static_tools(
            &self.project_paths,
            sub_memory.clone(),
            sub_long_term.clone(),
            sub_skills.clone(),
            Arc::clone(&self.config.invariants),
            sub_spawner,
            Arc::clone(&self.config.roles),
            sub_depth,
        );

        // 7. Apply tool filtering if the role restricts tools or delegation
        let needs_filtering = !role.tools.is_empty() || !role.can_delegate;
        let filtered_static_tools: Arc<CompositeToolRegistry> = if needs_filtering {
            let mut allowed: HashSet<String> = if !role.tools.is_empty() {
                role.tools.iter().cloned().collect()
            } else {
                sub_static_tools.definitions().iter().map(|d| d.schema.name.clone()).collect()
            };
            if !role.can_delegate {
                allowed.remove("delegate");
            }
            let filtered = OwnedFilteredToolRegistry::new(sub_static_tools, allowed);
            Arc::new(CompositeToolRegistry::new(vec![Box::new(filtered)]))
        } else {
            sub_static_tools
        };

        // 8. Push task as Goal to sub-agent's queue
        let correlation_id = format!("sub-{}", short_uuid);
        let body = MessageComposer::compose_task_body(task);
        let msg = QueueMessage {
            headers: MessageHeaders {
                message_type: MessageType::Goal,
                correlation_id: correlation_id.clone(),
                depth: 0,
                retry_count: 0,
                depends_on: vec![],
            },
            body,
        };
        sub_queue.push(msg).await.map_err(|e| ToolError::Execution(format!("failed to push goal: {e}")))?;

        // 9. Build WorkerContext and spawn concurrent workers
        let ctx = WorkerContext {
            queue: sub_queue.clone(),
            tools: filtered_static_tools,
            llm: sub_llm,
            memory: sub_memory,
            long_term: sub_long_term,
            skills: sub_skills,
            persona: Arc::new(persona.to_string()),
            limits: WorkerLimits {
                max_depth: role.max_depth,
                max_retries: role.max_retries,
                max_tool_result_bytes: self.config.max_tool_result_bytes,
                max_output_tokens: self.config.max_output_tokens,
                max_turns: self.config.max_turns,
                max_empty_cycles: self.config.max_empty_cycles,
            },
            estimator: Arc::clone(&self.estimator),
            compaction: self.config.compaction.clone(),
            consolidation: self.config.consolidation.clone(),
            hooks: sub_hooks,
        };

        let num_workers = role.max_concurrent.max(1);
        let mut handles = Vec::with_capacity(num_workers);
        for worker_id in 0..num_workers {
            let ctx = ctx.clone();
            handles.push(tokio::spawn(async move {
                worker_loop(ctx, worker_id).await
            }));
        }
        for handle in handles {
            handle
                .await
                .map_err(|e| ToolError::Execution(format!("sub-agent worker panicked: {e}")))?
                .map_err(|e| ToolError::Execution(format!("sub-agent worker failed: {e}")))?;
        }

        // 10. Collect results from done/ and failed/
        let done_metas = sub_queue.list_with_state("done").await
            .map_err(|e| ToolError::Execution(format!("failed to list done: {e}")))?;
        let failed_metas = sub_queue.list_with_state("failed").await
            .map_err(|e| ToolError::Execution(format!("failed to list failed: {e}")))?;

        let mut done_bodies = Vec::new();
        for meta in &done_metas {
            if let Ok(body) = sub_queue.read_done_body(&meta.id).await {
                done_bodies.push(body);
            }
        }

        let mut failed_bodies = Vec::new();
        for meta in &failed_metas {
            // Read failed message bodies from failed/ directory
            let failed_path = sub_queue_dir.join("failed").join(format!("{}.md", meta.id));
            if let Ok(content) = tokio::fs::read_to_string(&failed_path).await {
                // Extract body after the header separator
                if let Some(pos) = content.find("\n\n") {
                    failed_bodies.push(content[pos + 2..].to_string());
                } else {
                    failed_bodies.push(content);
                }
            }
        }

        // 11. Cleanup ephemeral dirs (best-effort)
        let _ = tokio::fs::remove_dir_all(&sub_queue_dir).await;
        let _ = tokio::fs::remove_dir_all(&sub_memory_dir).await;
        // Clean up isolated long-term memory if used
        if role.memory == MemoryMode::Isolated {
            let isolated_dir = self.project_paths.long_term_dir.join(format!("sub-{}", short_uuid));
            let _ = tokio::fs::remove_dir_all(&isolated_dir).await;
        }

        Ok(SubAgentResult {
            done_bodies,
            failed_bodies,
        })
    }
}
