use crate::domain::SkillManifest;
use serde::{Deserialize, Serialize};

/// The full agent configuration file (agent.toml).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentConfig {
    #[serde(default)]
    pub agent: AgentSection,

    #[serde(default)]
    pub roles: Vec<RoleProfile>,

    #[serde(default)]
    pub context_budget: ContextBudgetSection,

    #[serde(default)]
    pub consolidation: ConsolidationSection,

    #[serde(default)]
    pub fitness: FitnessSection,

    #[serde(default)]
    pub compaction: CompactionSection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentSection {
    /// LLM provider name ("anthropic", "openai", "openrouter").
    #[serde(default = "default_provider")]
    pub provider: String,

    /// LLM model identifier.
    #[serde(default = "default_model")]
    pub model: String,

    /// Custom base URL for the LLM API (e.g. OpenRouter endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Maximum concurrent workers.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Maximum subtask recursion depth.
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,

    /// Maximum retries per task.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Maximum tokens per LLM completion response.
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,

    /// Maximum number of LLM turns in a single agentic loop.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,

    /// Number of empty poll cycles before a worker gives up.
    #[serde(default = "default_max_empty_cycles")]
    pub max_empty_cycles: u32,

    /// Number of days to retain scratchpad files before garbage collection.
    #[serde(default = "default_scratchpad_retention_days")]
    pub scratchpad_retention_days: u32,

    /// Maximum tool calls allowed per single LLM turn.
    #[serde(default = "default_max_tool_calls_per_turn")]
    pub max_tool_calls_per_turn: usize,

    /// Maximum total tool calls across all turns in a single agentic loop.
    #[serde(default = "default_max_total_tool_calls")]
    pub max_total_tool_calls: usize,

    /// Enable prompt caching hints for static system prompt and tool blocks.
    #[serde(default = "default_true")]
    pub prompt_caching: bool,

    /// Enabled tool names (empty = all available).
    #[serde(default)]
    pub tools: Vec<String>,
}

/// A named role profile for sub-agent delegation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoleProfile {
    /// Unique role name (e.g. "coder", "reviewer").
    pub name: String,

    /// Persona filename in prompts/ directory. None = use parent's persona.
    #[serde(default)]
    pub persona: Option<String>,

    /// LLM model override. None = use parent's model.
    #[serde(default)]
    pub model: Option<String>,

    /// Tool names this role can use. Empty = all available.
    #[serde(default)]
    pub tools: Vec<String>,

    /// Maximum subtask recursion depth for this role.
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,

    /// Maximum retries per task for this role.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Maximum concurrent workers for this role.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Memory sharing mode: shared (uses parent's long-term memory) or isolated.
    #[serde(default)]
    pub memory: MemoryMode,

    /// Whether this role can spawn further sub-agents via delegate.
    #[serde(default = "default_true")]
    pub can_delegate: bool,

    /// Skill manifests attached to this role (lazy-loaded on demand).
    #[serde(default)]
    pub skills: Vec<SkillManifest>,
}

/// Controls how a sub-agent's long-term memory relates to the parent's.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryMode {
    /// Sub-agent shares the parent's long-term memory store.
    #[default]
    Shared,
    /// Sub-agent gets its own isolated memory store.
    Isolated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextBudgetSection {
    /// Maximum tokens for message body content.
    #[serde(default = "default_max_body_tokens")]
    pub max_body_tokens: usize,

    /// Maximum tokens for tool results before spilling.
    #[serde(default = "default_max_tool_result_tokens")]
    pub max_tool_result_tokens: usize,

    /// Maximum tokens for tool inputs before rewriting in history.
    #[serde(default = "default_max_tool_input_tokens")]
    pub max_tool_input_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConsolidationSection {
    /// Whether to consolidate learnings after task completion.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Whether to extract facts from task results.
    #[serde(default = "default_true")]
    pub extract_facts: bool,

    /// Whether to extract/update skills from task results.
    #[serde(default = "default_true")]
    pub extract_skills: bool,

    /// Whether to extract/update strategies from task results.
    #[serde(default = "default_true")]
    pub extract_strategies: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FitnessSection {
    /// Minimum pass rate to consider a strategy fit (0.0 - 1.0).
    #[serde(default = "default_min_pass_rate")]
    pub min_pass_rate: f64,

    /// Minimum number of uses before judging fitness.
    #[serde(default = "default_min_uses")]
    pub min_uses: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionSection {
    /// Number of recent turns to preserve intact (each turn = user + assistant).
    #[serde(default = "default_compaction_preserve_turns")]
    pub preserve_turns: usize,

    /// Maximum tokens for the LLM-generated compaction summary.
    #[serde(default = "default_compaction_max_summary_tokens")]
    pub max_summary_tokens: u32,

    /// Whether to spill full conversation history to disk before compaction.
    #[serde(default = "default_true")]
    pub spill_history: bool,
}

// ── Defaults ─────────────────────────────────────────────────────

fn default_provider() -> String {
    "anthropic".to_string()
}
fn default_model() -> String {
    "claude-sonnet-4-20250514".to_string()
}
fn default_max_concurrent() -> usize {
    1
}
fn default_max_depth() -> u32 {
    3
}
fn default_max_retries() -> u32 {
    3
}
fn default_max_output_tokens() -> u32 {
    16_384
}
fn default_max_turns() -> usize {
    32
}
fn default_max_empty_cycles() -> u32 {
    300
}
fn default_scratchpad_retention_days() -> u32 {
    7
}
fn default_max_tool_calls_per_turn() -> usize {
    64
}
fn default_max_total_tool_calls() -> usize {
    512
}
fn default_max_body_tokens() -> usize {
    50_000
}
fn default_max_tool_result_tokens() -> usize {
    10_000
}
fn default_max_tool_input_tokens() -> usize {
    10_000
}
fn default_true() -> bool {
    true
}
fn default_min_pass_rate() -> f64 {
    0.6
}
fn default_min_uses() -> u32 {
    3
}
fn default_compaction_preserve_turns() -> usize {
    6
}
fn default_compaction_max_summary_tokens() -> u32 {
    1024
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent: AgentSection::default(),
            roles: default_roles(),
            context_budget: ContextBudgetSection::default(),
            consolidation: ConsolidationSection::default(),
            fitness: FitnessSection::default(),
            compaction: CompactionSection::default(),
        }
    }
}

fn default_roles() -> Vec<RoleProfile> {
    vec![
        RoleProfile {
            name: "coder".into(),
            persona: Some("coder.md".into()),
            model: None,
            tools: vec![
                "shell_exec".into(),
                "file_read".into(),
                "file_write".into(),
                "working_memory_read".into(),
                "working_memory_update".into(),
                "memory_query_facts".into(),
                "memory_store_fact".into(),
            ],
            max_depth: 1,
            max_retries: 3,
            max_concurrent: 1,
            memory: MemoryMode::Shared,
            can_delegate: false,
            skills: vec![],
        },
        RoleProfile {
            name: "reviewer".into(),
            persona: Some("reviewer.md".into()),
            model: None,
            tools: vec![
                "shell_exec".into(),
                "file_read".into(),
                "memory_query_facts".into(),
            ],
            max_depth: 1,
            max_retries: 2,
            max_concurrent: 1,
            memory: MemoryMode::Shared,
            can_delegate: false,
            skills: vec![],
        },
    ]
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: default_model(),
            base_url: None,
            max_concurrent: default_max_concurrent(),
            max_depth: default_max_depth(),
            max_retries: default_max_retries(),
            max_output_tokens: default_max_output_tokens(),
            max_turns: default_max_turns(),
            max_empty_cycles: default_max_empty_cycles(),
            scratchpad_retention_days: default_scratchpad_retention_days(),
            max_tool_calls_per_turn: default_max_tool_calls_per_turn(),
            max_total_tool_calls: default_max_total_tool_calls(),
            prompt_caching: true,
            tools: vec![],
        }
    }
}

impl Default for ContextBudgetSection {
    fn default() -> Self {
        Self {
            max_body_tokens: default_max_body_tokens(),
            max_tool_result_tokens: default_max_tool_result_tokens(),
            max_tool_input_tokens: default_max_tool_input_tokens(),
        }
    }
}

impl Default for ConsolidationSection {
    fn default() -> Self {
        Self {
            enabled: true,
            extract_facts: true,
            extract_skills: true,
            extract_strategies: true,
        }
    }
}

impl Default for FitnessSection {
    fn default() -> Self {
        Self {
            min_pass_rate: default_min_pass_rate(),
            min_uses: default_min_uses(),
        }
    }
}

impl Default for CompactionSection {
    fn default() -> Self {
        Self {
            preserve_turns: default_compaction_preserve_turns(),
            max_summary_tokens: default_compaction_max_summary_tokens(),
            spill_history: true,
        }
    }
}

impl AgentConfig {
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        let config: AgentConfig = toml::from_str(s)?;
        Ok(config)
    }

    pub fn to_toml(&self) -> anyhow::Result<String> {
        let s = toml::to_string_pretty(self)?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let config = AgentConfig::default();
        let toml_str = config.to_toml().unwrap();
        let parsed = AgentConfig::from_toml(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn partial_toml_uses_defaults() {
        let toml_str = r#"
[agent]
max_depth = 5
"#;
        let config = AgentConfig::from_toml(toml_str).unwrap();
        assert_eq!(config.agent.max_depth, 5);
        assert_eq!(config.agent.max_concurrent, 1); // default
        assert_eq!(config.agent.max_retries, 3); // default
        assert!(config.roles.is_empty()); // default
    }
}
