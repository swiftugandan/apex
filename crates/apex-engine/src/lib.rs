mod agentic_loop;
mod claim_tool_factory;
mod compaction;
mod consolidation;
mod jit_retrieval;
pub mod log;
mod paths;
mod registry;
pub mod util;
mod worker;

#[cfg(test)]
pub(crate) mod test_mocks;

pub use agentic_loop::{run_agentic_loop, LoopConfig, LoopOutcome};
pub use claim_tool_factory::{ClaimContext, ClaimToolFactory};
pub use paths::ProjectPaths;
pub use registry::{CompositeToolRegistry, OwnedFilteredToolRegistry};
pub use worker::{worker_loop, WorkerContext, WorkerLimits};
