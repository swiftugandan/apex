mod agentic_loop;
mod consolidation;
mod delegate;
mod paths;
mod registry;
pub mod util;
mod worker;

pub use agentic_loop::run_agentic_loop;
pub use delegate::InProcessSpawner;
pub use paths::ProjectPaths;
pub use registry::{build_static_tools, ApexToolRegistry, CompositeToolRegistry, OwnedFilteredToolRegistry};
pub use worker::{worker_loop, WorkerContext};

// Re-export key types from apex-tools so downstream only needs apex-engine.
pub use apex_tools::{DelegateToolRegistry, SubAgentResult, SubAgentSpawner};
