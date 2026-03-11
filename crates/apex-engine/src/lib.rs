mod agentic_loop;
mod compaction;
mod consolidation;
pub mod constants;
mod delegate;
pub mod log;
mod paths;
mod registry;
pub mod util;
mod worker;

#[cfg(test)]
pub(crate) mod test_mocks;

pub use agentic_loop::{run_agentic_loop, LoopConfig, LoopOutcome};
pub use delegate::{InfraFactories, InProcessSpawner, SpawnerConfig};
pub use paths::ProjectPaths;
pub use registry::{build_static_tools, ApexToolRegistry, CompositeToolRegistry, OwnedFilteredToolRegistry};
pub use worker::{worker_loop, WorkerContext};

