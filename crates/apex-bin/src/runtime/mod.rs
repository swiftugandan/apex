use std::sync::Arc;

use apex_core::config::{Invariants, RoleProfile, ToolLoadingSection};
use apex_core::ports::{MemoryStore, SkillStore, SubAgentSpawner, WorkingMemory};

use apex_engine::{CompositeToolRegistry, ProjectPaths};
use apex_tools::spill::SpillManager;
use apex_tools::{
    BuiltinToolRegistry, ConfigToolRegistry, CustomToolRegistry, DelegateToolRegistry,
    HooksToolRegistry, MemoryToolRegistry, SkillToolRegistry,
};

pub mod orientation;
pub mod session_memory;
pub mod spawner;
pub mod tool_factory;

pub use orientation::ScratchpadOrientationFactory;
pub use spawner::{InProcessSpawner, SpawnerConfig, SubAgentRuntimeBuilder};
pub use tool_factory::CliClaimToolFactory;

/// Build the static tool registries (Builtin, Memory, Custom, Config, Delegate, Hooks, Skills)
/// once per process or sub-agent lifetime.
#[allow(clippy::too_many_arguments)]
pub fn build_static_tools(
    paths: &ProjectPaths,
    memory: Arc<dyn WorkingMemory>,
    long_term: Arc<dyn MemoryStore>,
    skills: Arc<dyn SkillStore>,
    invariants: Arc<Invariants>,
    spawner: Arc<dyn SubAgentSpawner>,
    roles: Arc<[RoleProfile]>,
    remaining_delegate_depth: u32,
    tool_loading: &ToolLoadingSection,
) -> Arc<CompositeToolRegistry> {
    let memory_tools = MemoryToolRegistry::new(memory, long_term.clone());
    let custom_spill = SpillManager::new(paths.scratch_dir.clone());
    let custom_tools =
        CustomToolRegistry::new(paths.tools_dir.clone(), custom_spill, Some(skills.clone()));
    let config_tools = ConfigToolRegistry::new(paths.config_dir.clone(), Arc::clone(&invariants));
    let delegate_tools = DelegateToolRegistry::new(
        roles,
        paths.prompts_dir.clone(),
        spawner,
        remaining_delegate_depth,
    );
    let hooks_tools = HooksToolRegistry::new(paths.hooks_dir.clone());
    let skill_tools = SkillToolRegistry::new(skills);
    Arc::new(CompositeToolRegistry::with_config(
        vec![
            Box::new(BuiltinToolRegistry::new(paths.scratch_dir.clone())),
            Box::new(memory_tools),
            Box::new(custom_tools),
            Box::new(config_tools),
            Box::new(delegate_tools),
            Box::new(hooks_tools),
            Box::new(skill_tools),
        ],
        tool_loading,
    ))
}
