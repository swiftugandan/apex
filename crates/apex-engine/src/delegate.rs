use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use async_trait::async_trait;

use apex_core::config::{Invariants, MemoryMode, RoleProfile};
use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::domain::{MessageHeaders, MessageType, QueueMessage};
use apex_core::error::ToolError;
use apex_core::ports::{LlmProvider, MemoryStore, Queue, ToolRegistry, WorkingMemory};

use apex_tools::{SubAgentResult, SubAgentSpawner};

use crate::paths::ProjectPaths;
use crate::registry::{build_static_tools, CompositeToolRegistry, OwnedFilteredToolRegistry};
use crate::worker::{worker_loop, WorkerContext};

/// Concrete SubAgentSpawner that runs sub-agents in-process with their own
/// queue, working memory, and (optionally isolated) long-term memory.
pub struct InProcessSpawner {
    pub project_paths: ProjectPaths,
    pub parent_long_term: Arc<dyn MemoryStore>,
    pub llm: Arc<dyn LlmProvider>,
    pub estimator: Arc<Mutex<TokenEstimator>>,
    pub invariants: Arc<Invariants>,
    pub roles: Vec<RoleProfile>,
    pub max_tool_result_bytes: usize,
    pub remaining_delegate_depth: u32,
    /// Factory functions for creating infra components.
    /// These closures let the engine create concrete adapters without depending on apex-infra.
    pub queue_factory: Arc<dyn Fn(&std::path::Path) -> Result<Arc<dyn Queue>, String> + Send + Sync>,
    pub working_memory_factory: Arc<dyn Fn(&std::path::Path) -> Arc<dyn WorkingMemory> + Send + Sync>,
    pub memory_store_factory: Arc<dyn Fn(&std::path::Path) -> Result<Arc<dyn MemoryStore>, String> + Send + Sync>,
}

#[async_trait]
impl SubAgentSpawner for InProcessSpawner {
    async fn spawn(
        &self,
        task: &str,
        role: &RoleProfile,
        persona: &str,
    ) -> Result<SubAgentResult, ToolError> {
        let short_uuid = {
            use std::time::{SystemTime, UNIX_EPOCH};
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let pid = std::process::id();
            format!("{:08x}{:04x}", (t & 0xFFFF_FFFF) as u32, (pid & 0xFFFF) as u32)
        };

        // 1. Create ephemeral queue
        let sub_queue_dir = self.project_paths.queues_dir.join(format!("sub-{}-{}", role.name, short_uuid));
        let sub_queue = (self.queue_factory)(&sub_queue_dir)
            .map_err(|e| ToolError::Execution(format!("failed to create sub-agent queue: {e}")))?;

        // 2. Create ephemeral working memory directory
        let sub_memory_dir = self.project_paths.working_memory.join(format!("sub-{}", short_uuid));
        std::fs::create_dir_all(&sub_memory_dir)
            .map_err(|e| ToolError::Execution(format!("failed to create sub-agent memory dir: {e}")))?;
        let sub_memory = (self.working_memory_factory)(&sub_memory_dir);

        // 3. Resolve long-term memory based on role's memory mode
        let sub_long_term: Arc<dyn MemoryStore> = match role.memory {
            MemoryMode::Shared => Arc::clone(&self.parent_long_term),
            MemoryMode::Isolated => {
                let isolated_dir = self.project_paths.long_term_dir.join(format!("sub-{}", short_uuid));
                let db_path = isolated_dir.join("memory.db");
                (self.memory_store_factory)(&db_path)
                    .map_err(|e| ToolError::Execution(format!("failed to create isolated memory: {e}")))?
            }
        };

        // 4. Resolve LLM — use role's model override if different
        // For now, we always use the parent's LLM since creating a new provider
        // requires infra knowledge. The model override would need the factory pattern too.
        let sub_llm = Arc::clone(&self.llm);

        // 5. Build a new spawner for sub-sub-agents (with decremented depth)
        let sub_spawner: Arc<dyn SubAgentSpawner> = Arc::new(InProcessSpawner {
            project_paths: self.project_paths.clone(),
            parent_long_term: sub_long_term.clone(),
            llm: sub_llm.clone(),
            estimator: Arc::clone(&self.estimator),
            invariants: Arc::clone(&self.invariants),
            roles: self.roles.clone(),
            max_tool_result_bytes: self.max_tool_result_bytes,
            remaining_delegate_depth: if role.can_delegate {
                self.remaining_delegate_depth.saturating_sub(1)
            } else {
                0
            },
            queue_factory: Arc::clone(&self.queue_factory),
            working_memory_factory: Arc::clone(&self.working_memory_factory),
            memory_store_factory: Arc::clone(&self.memory_store_factory),
        });

        // 6. Build static tools for sub-agent
        let sub_static_tools = build_static_tools(
            &self.project_paths,
            sub_memory.clone(),
            sub_long_term.clone(),
            Arc::clone(&self.invariants),
            sub_spawner,
            self.max_tool_result_bytes,
            self.roles.clone(),
            if role.can_delegate { self.remaining_delegate_depth.saturating_sub(1) } else { 0 },
        );

        // 7. Apply tool filtering if the role specifies a tools list
        let filtered_static_tools: Arc<dyn apex_core::ports::ToolRegistry> = if !role.tools.is_empty() {
            let mut allowed: HashSet<String> = role.tools.iter().cloned().collect();
            if !role.can_delegate {
                allowed.remove("delegate");
            }
            let filtered = OwnedFilteredToolRegistry::new(sub_static_tools, allowed);
            Arc::new(CompositeToolRegistry::new(vec![Box::new(filtered)]))
        } else if !role.can_delegate {
            let mut allowed: HashSet<String> = sub_static_tools
                .definitions()
                .iter()
                .map(|d| d.schema.name.clone())
                .collect();
            allowed.remove("delegate");
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

        // 9. Build WorkerContext and run worker_loop
        let ctx = WorkerContext {
            queue: sub_queue.clone(),
            tools: filtered_static_tools,
            llm: sub_llm,
            memory: sub_memory,
            long_term: sub_long_term,
            persona: Arc::new(persona.to_string()),
            max_depth: role.max_depth,
            max_retries: role.max_retries,
            max_tool_result_bytes: self.max_tool_result_bytes,
            estimator: Arc::clone(&self.estimator),
        };

        worker_loop(ctx, 0).await.map_err(|e| ToolError::Execution(format!("sub-agent worker failed: {e}")))?;

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
            if let Ok(content) = std::fs::read_to_string(&failed_path) {
                // Extract body after the header separator
                if let Some(pos) = content.find("\n\n") {
                    failed_bodies.push(content[pos + 2..].to_string());
                } else {
                    failed_bodies.push(content);
                }
            }
        }

        // 11. Cleanup ephemeral dirs (best-effort)
        let _ = std::fs::remove_dir_all(&sub_queue_dir);
        let _ = std::fs::remove_dir_all(&sub_memory_dir);
        // Clean up isolated long-term memory if used
        if role.memory == MemoryMode::Isolated {
            let isolated_dir = self.project_paths.long_term_dir.join(format!("sub-{}", short_uuid));
            let _ = std::fs::remove_dir_all(&isolated_dir);
        }

        Ok(SubAgentResult {
            done_bodies,
            failed_bodies,
        })
    }
}
