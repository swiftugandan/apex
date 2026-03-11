use std::sync::Arc;
use tokio::sync::Mutex;

use async_trait::async_trait;

use apex_core::domain::Scratchpad;
use apex_core::ports::{HookRegistry, MemoryStore, Queue, SkillStore, ToolRegistry, WorkingMemory};

// ── ClaimContext ────────────────────────────────────────────────────

/// All context needed to build per-claim tools.
pub struct ClaimContext {
    pub queue: Arc<dyn Queue>,
    pub correlation_id: String,
    pub current_depth: u32,
    pub max_depth: u32,
    pub parent_goal: String,
    pub parent_body: String,
    pub long_term: Arc<dyn MemoryStore>,
    pub skills: Arc<dyn SkillStore>,
    pub memory: Arc<dyn WorkingMemory>,
    pub scratchpad: Arc<Mutex<Scratchpad>>,
    pub hooks: Option<Arc<dyn HookRegistry>>,
}

// ── ClaimToolFactory trait ──────────────────────────────────────────

/// Factory for building per-claim tool registries.
///
/// The engine calls `build()` once per claimed message. Concrete
/// implementations live in `apex-bin` (the assembly crate); the engine
/// depends only on this trait.
#[async_trait]
pub trait ClaimToolFactory: Send + Sync {
    async fn build(&self, ctx: &ClaimContext) -> Box<dyn ToolRegistry>;
}
