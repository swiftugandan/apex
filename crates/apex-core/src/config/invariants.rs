use serde::{Deserialize, Serialize};

/// Operator-defined ceilings that the agent cannot exceed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Invariants {
    #[serde(default)]
    pub limits: InvariantLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InvariantLimits {
    /// Maximum recursion depth for subtask chains.
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,

    /// Maximum concurrent workers.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Maximum number of custom tools.
    #[serde(default = "default_max_tools")]
    pub max_tools: usize,

    /// Maximum body tokens for context budget.
    #[serde(default = "default_max_body_tokens")]
    pub max_body_tokens: usize,

    /// Maximum retries per task.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Maximum nesting depth for sub-agent delegation.
    #[serde(default = "default_max_sub_agent_depth")]
    pub max_sub_agent_depth: u32,
}

fn default_max_depth() -> u32 {
    5
}
fn default_max_concurrent() -> usize {
    8
}
fn default_max_tools() -> usize {
    50
}
fn default_max_body_tokens() -> usize {
    100_000
}
fn default_max_retries() -> u32 {
    10
}
fn default_max_sub_agent_depth() -> u32 {
    2
}

impl Default for InvariantLimits {
    fn default() -> Self {
        Self {
            max_depth: default_max_depth(),
            max_concurrent: default_max_concurrent(),
            max_tools: default_max_tools(),
            max_body_tokens: default_max_body_tokens(),
            max_retries: default_max_retries(),
            max_sub_agent_depth: default_max_sub_agent_depth(),
        }
    }
}

impl Default for Invariants {
    fn default() -> Self {
        Self {
            limits: InvariantLimits::default(),
        }
    }
}

impl Invariants {
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        let inv: Invariants = toml::from_str(s)?;
        Ok(inv)
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
        let inv = Invariants::default();
        let toml_str = inv.to_toml().unwrap();
        let parsed = Invariants::from_toml(&toml_str).unwrap();
        assert_eq!(inv, parsed);
    }

    #[test]
    fn partial_toml_uses_defaults() {
        let toml_str = r#"
[limits]
max_depth = 3
"#;
        let inv = Invariants::from_toml(toml_str).unwrap();
        assert_eq!(inv.limits.max_depth, 3);
        assert_eq!(inv.limits.max_concurrent, 8); // default
    }
}
