use std::sync::Arc;
use tokio::sync::Mutex;

use async_trait::async_trait;

use apex_core::context::TokenEstimator;
use apex_core::domain::{ToolCall, ToolDef, ToolResult};
use apex_core::error::ToolError;
use apex_core::ports::ToolRegistry;
use serde_json::Value;

use apex_engine::util::composer_from_estimator;
use apex_engine::{ClaimContext, ClaimToolFactory, CompositeToolRegistry};
use apex_tools::{MemoryToolRegistry, QueueToolRegistry};

use super::session_memory::SessionWorkingMemory;

// ── SharedToolRegistry ──────────────────────────────────────────────

/// Thin adapter: wraps `Arc<CompositeToolRegistry>` so it can be placed
/// inside another `CompositeToolRegistry` as `Box<dyn ToolRegistry>`.
struct SharedToolRegistry(Arc<CompositeToolRegistry>);

#[async_trait]
impl ToolRegistry for SharedToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        self.0.definitions()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        self.0.execute(call).await
    }

    fn rewrite_input(
        &self,
        call: &ToolCall,
        result: &ToolResult,
        max_bytes: usize,
    ) -> Option<Value> {
        self.0.rewrite_input(call, result, max_bytes)
    }
}

// ── CliClaimToolFactory ─────────────────────────────────────────────

/// Assembly-crate factory that builds per-claim tool registries.
///
/// For each claimed message it:
/// 1. Creates a [`SessionWorkingMemory`] that shares the agentic loop's
///    in-memory `Scratchpad` mutex while persisting to the backing store.
/// 2. Creates a [`MemoryToolRegistry`] wired to the session adapter,
///    so `working_memory_read`/`working_memory_update` operate on the
///    shared mutex (replacing the old engine-side interception).
/// 3. Creates a [`QueueToolRegistry`] for `decompose_goal`/`queue_read_done`.
/// 4. Composes everything with the pre-built static tools into a single
///    [`CompositeToolRegistry`].
pub struct CliClaimToolFactory {
    pub static_tools: Arc<CompositeToolRegistry>,
    pub estimator: Arc<Mutex<TokenEstimator>>,
}

#[async_trait]
impl ClaimToolFactory for CliClaimToolFactory {
    async fn build(&self, ctx: &ClaimContext) -> Box<dyn ToolRegistry> {
        // 1. Per-claim working memory: shares the loop's scratchpad mutex.
        let session_memory = Arc::new(SessionWorkingMemory::new(
            Arc::clone(&ctx.scratchpad),
            Arc::clone(&ctx.memory),
        ));

        // 2. Memory tools wired to the session adapter.
        let memory_tools = MemoryToolRegistry::new(
            session_memory,
            Arc::clone(&ctx.long_term),
            Arc::clone(&ctx.skills),
        );

        // 3. Queue tools for decompose_goal / queue_read_done.
        let composer = composer_from_estimator(&self.estimator).await;
        let queue_tools = QueueToolRegistry::new(
            Arc::clone(&ctx.queue),
            ctx.correlation_id.clone(),
            ctx.current_depth,
            ctx.max_depth,
            ctx.parent_goal.clone(),
            ctx.parent_body.clone(),
            Some(Arc::clone(&ctx.long_term)),
            Some(Arc::clone(&ctx.skills)),
            composer,
        )
        .with_hooks(ctx.hooks.clone());

        // 4. Compose: per-claim memory first (shadows the MemoryToolRegistry
        //    inside static_tools), then static tools, then queue tools.
        Box::new(CompositeToolRegistry::new(vec![
            Box::new(memory_tools),
            Box::new(SharedToolRegistry(Arc::clone(&self.static_tools))),
            Box::new(queue_tools),
        ]))
    }
}
