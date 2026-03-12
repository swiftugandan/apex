use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use async_trait::async_trait;

use apex_core::config::{
    CompactionSection, ConsolidationSection, Invariants, MemoryMode, RoleProfile,
};
use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::domain::{MessageHeaders, MessageType, QueueMessage, ToolCall, ToolDef, ToolResult};
use apex_core::error::ToolError;
use apex_core::ports::{
    HookRegistry, LlmProvider, MemoryStore, Queue, SkillStore, SubAgentResult, SubAgentSpawner,
    ToolRegistry, WorkingMemory,
};
use serde_json::Value;

use apex_engine::{
    worker_loop, ClaimContext, ClaimToolFactory, ProjectPaths, WorkerContext, WorkerLimits,
};
use apex_infra::{FsScratchpadStore, RfbmqAdapter, SqliteMemoryStore};
use apex_tools::FilteredHookRegistry;

use super::CliClaimToolFactory;

// ── SubAgentRuntimeBuilder ──────────────────────────────────────────

/// Typed builder for sub-agent infrastructure components, replacing
/// the old `InfraFactories` closure bag.  Lives in the assembly crate
/// where concrete infra types are directly available.
pub struct SubAgentRuntimeBuilder;

impl SubAgentRuntimeBuilder {
    pub fn build_queue(&self, path: &Path) -> Result<Arc<dyn Queue>, String> {
        RfbmqAdapter::init(path)
            .map(|a| Arc::new(a) as Arc<dyn Queue>)
            .map_err(|e| e.to_string())
    }

    pub fn build_working_memory(&self, path: &Path) -> Arc<dyn WorkingMemory> {
        Arc::new(FsScratchpadStore::new(path.to_path_buf()))
    }

    pub fn build_memory_store(&self, path: &Path) -> Result<Arc<dyn MemoryStore>, String> {
        SqliteMemoryStore::open(path)
            .map(|s| Arc::new(s) as Arc<dyn MemoryStore>)
            .map_err(|e| e.to_string())
    }
}

// ── FilteredToolRegistryBox ─────────────────────────────────────────
//
// Generic filter wrapper for any `Box<dyn ToolRegistry>`.
// Used by `FilteredClaimToolFactory` to filter the *combined* registry
// output (static + per-claim memory + queue tools).

struct FilteredToolRegistryBox {
    inner: Box<dyn ToolRegistry>,
    allowed: HashSet<String>,
}

#[async_trait]
impl ToolRegistry for FilteredToolRegistryBox {
    fn definitions(&self) -> Vec<ToolDef> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|d| self.allowed.contains(&d.schema.name))
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        if !self.allowed.contains(&call.name) {
            return Err(ToolError::UnknownTool(call.name.clone()));
        }
        self.inner.execute(call).await
    }

    fn rewrite_input(
        &self,
        call: &ToolCall,
        result: &ToolResult,
        max_bytes: usize,
    ) -> Option<Value> {
        if self.allowed.contains(&call.name) {
            self.inner.rewrite_input(call, result, max_bytes)
        } else {
            None
        }
    }
}

// ── FilteredClaimToolFactory ────────────────────────────────────────

/// Wraps a `CliClaimToolFactory` and applies an allow-list filter to
/// the *combined* registry it produces.  This ensures role-based
/// filtering covers all tools — static, per-claim memory, and queue —
/// not just static tools (fixing the gap addressed in 3C.3).
struct FilteredClaimToolFactory {
    inner: CliClaimToolFactory,
    allowed: HashSet<String>,
}

#[async_trait]
impl ClaimToolFactory for FilteredClaimToolFactory {
    async fn build(&self, ctx: &ClaimContext) -> Box<dyn ToolRegistry> {
        let registry = self.inner.build(ctx).await;
        Box::new(FilteredToolRegistryBox {
            inner: registry,
            allowed: self.allowed.clone(),
        })
    }
}

// ── SpawnerConfig ───────────────────────────────────────────────────

/// Static configuration for sub-agent spawning.
pub struct SpawnerConfig {
    pub invariants: Arc<Invariants>,
    pub roles: Arc<[RoleProfile]>,
    pub max_tool_result_bytes: usize,
    pub max_tool_input_bytes: usize,
    pub max_output_tokens: u32,
    pub remaining_delegate_depth: u32,
    pub max_turns: usize,
    pub max_empty_cycles: u32,
    pub compaction: CompactionSection,
    pub consolidation: ConsolidationSection,
    pub max_tool_calls_per_turn: usize,
    pub max_total_tool_calls: usize,
    pub prompt_caching: bool,
}

// ── InProcessSpawner ────────────────────────────────────────────────

/// Concrete `SubAgentSpawner` that runs sub-agents in-process with
/// their own queue, working memory, and (optionally isolated) long-term
/// memory.  Uses `SubAgentRuntimeBuilder` to create infra components
/// and `CliClaimToolFactory` for per-claim tool assembly.
pub struct InProcessSpawner {
    pub project_paths: ProjectPaths,
    pub parent_long_term: Arc<dyn MemoryStore>,
    pub parent_skills: Arc<dyn SkillStore>,
    pub llm: Arc<dyn LlmProvider>,
    pub estimator: Arc<Mutex<TokenEstimator>>,
    pub config: SpawnerConfig,
    pub runtime: Arc<SubAgentRuntimeBuilder>,
    /// Optional hooks from the parent agent.  Only hooks with
    /// `propagate: true` are inherited by sub-agents.
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
        let sub_queue_dir = self
            .project_paths
            .queues_dir
            .join(format!("sub-{}", short_uuid));
        let sub_queue = self
            .runtime
            .build_queue(&sub_queue_dir)
            .map_err(|e| ToolError::Execution(format!("failed to create sub-agent queue: {e}")))?;

        // 2. Create ephemeral working memory directory
        let sub_memory_dir = self
            .project_paths
            .working_memory
            .join(format!("sub-{}", short_uuid));
        tokio::fs::create_dir_all(&sub_memory_dir)
            .await
            .map_err(|e| {
                ToolError::Execution(format!("failed to create sub-agent memory dir: {e}"))
            })?;
        let sub_memory = self.runtime.build_working_memory(&sub_memory_dir);

        // 3. Resolve long-term memory based on role's memory mode
        let sub_long_term: Arc<dyn MemoryStore> = match role.memory {
            MemoryMode::Shared => Arc::clone(&self.parent_long_term),
            MemoryMode::Isolated => {
                let isolated_dir = self
                    .project_paths
                    .long_term_dir
                    .join(format!("sub-{}", short_uuid));
                let db_path = isolated_dir.join("memory.db");
                self.runtime.build_memory_store(&db_path).map_err(|e| {
                    ToolError::Execution(format!("failed to create isolated memory: {e}"))
                })?
            }
        };

        // 3b. Skills are always shared (they're files on disk)
        let sub_skills: Arc<dyn SkillStore> = Arc::clone(&self.parent_skills);

        // 4. Resolve LLM — use parent's for now (model override needs factory pattern)
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
                max_tool_input_bytes: self.config.max_tool_input_bytes,
                max_output_tokens: self.config.max_output_tokens,
                remaining_delegate_depth: sub_depth,
                max_turns: self.config.max_turns,
                max_empty_cycles: self.config.max_empty_cycles,
                compaction: self.config.compaction.clone(),
                consolidation: self.config.consolidation.clone(),
                max_tool_calls_per_turn: self.config.max_tool_calls_per_turn,
                max_total_tool_calls: self.config.max_total_tool_calls,
                prompt_caching: self.config.prompt_caching,
            },
            runtime: Arc::clone(&self.runtime),
            hooks: sub_hooks.clone(),
        });

        // 6. Build static tools for sub-agent
        let sub_static_tools = super::build_static_tools(
            &self.project_paths,
            sub_memory.clone(),
            sub_long_term.clone(),
            sub_skills.clone(),
            Arc::clone(&self.config.invariants),
            sub_spawner,
            Arc::clone(&self.config.roles),
            sub_depth,
        );

        // 7. Build claim tool factory, with role-based filtering if needed.
        //
        //    3C.3: Filtering wraps the *combined* registry (static + per-claim
        //    memory + queue tools) via FilteredClaimToolFactory, so queue tools
        //    and per-claim memory tools are also subject to role restrictions.
        let base_factory = CliClaimToolFactory {
            static_tools: Arc::clone(&sub_static_tools),
            estimator: Arc::clone(&self.estimator),
        };

        let needs_filtering = !role.tools.is_empty() || !role.can_delegate;
        let claim_factory: Arc<dyn ClaimToolFactory> = if needs_filtering {
            let mut allowed: HashSet<String> = if !role.tools.is_empty() {
                role.tools.iter().cloned().collect()
            } else {
                // Start with all static tool names
                let mut names: HashSet<String> = sub_static_tools
                    .definitions()
                    .iter()
                    .map(|d| d.schema.name.clone())
                    .collect();
                // Include per-claim queue tool names that aren't in static tools
                names.insert("decompose_goal".to_string());
                names.insert("queue_read_done".to_string());
                names
            };
            if !role.can_delegate {
                allowed.remove("delegate");
            }
            Arc::new(FilteredClaimToolFactory {
                inner: base_factory,
                allowed,
            })
        } else {
            Arc::new(base_factory)
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
                skills: role.skills.clone(),
            },
            body,
        };
        sub_queue
            .push(msg)
            .await
            .map_err(|e| ToolError::Execution(format!("failed to push goal: {e}")))?;

        // 9. Build WorkerContext and spawn concurrent workers
        let ctx = WorkerContext {
            queue: sub_queue.clone(),
            claim_tool_factory: claim_factory,
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
                max_tool_input_bytes: self.config.max_tool_input_bytes,
                max_tool_calls_per_turn: self.config.max_tool_calls_per_turn,
                max_total_tool_calls: self.config.max_total_tool_calls,
                prompt_caching: self.config.prompt_caching,
            },
            estimator: Arc::clone(&self.estimator),
            compaction: self.config.compaction.clone(),
            consolidation: self.config.consolidation.clone(),
            hooks: sub_hooks,
            scratch_dir: Some(self.project_paths.scratch_dir.clone()),
        };

        let num_workers = role.max_concurrent.max(1);
        let mut handles = Vec::with_capacity(num_workers);
        for worker_id in 0..num_workers {
            let ctx = ctx.clone();
            handles.push(tokio::spawn(
                async move { worker_loop(ctx, worker_id).await },
            ));
        }
        for handle in handles {
            handle
                .await
                .map_err(|e| ToolError::Execution(format!("sub-agent worker panicked: {e}")))?
                .map_err(|e| ToolError::Execution(format!("sub-agent worker failed: {e}")))?;
        }

        // 10. Collect results from done/ and failed/
        let done_metas = sub_queue
            .list_with_state("done")
            .await
            .map_err(|e| ToolError::Execution(format!("failed to list done: {e}")))?;
        let failed_metas = sub_queue
            .list_with_state("failed")
            .await
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
            let isolated_dir = self
                .project_paths
                .long_term_dir
                .join(format!("sub-{}", short_uuid));
            let _ = tokio::fs::remove_dir_all(&isolated_dir).await;
        }

        Ok(SubAgentResult {
            done_bodies,
            failed_bodies,
        })
    }
}
