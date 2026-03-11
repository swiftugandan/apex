mod agent;
mod invariants;
mod loader;
mod validate;

pub use agent::{AgentConfig, CompactionSection, ConsolidationSection, MemoryMode, RoleProfile};
pub use invariants::Invariants;
pub use loader::ConfigLoader;
pub use validate::{validate_against_invariants, validate_full, ValidationIssue, ValidationReport};
