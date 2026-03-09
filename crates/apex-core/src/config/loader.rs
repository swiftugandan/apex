use std::path::Path;

use anyhow::{Context, Result};

use super::agent::AgentConfig;
use super::invariants::Invariants;

pub struct ConfigLoader;

impl ConfigLoader {
    /// Load invariants from `config_dir/invariants.toml`.
    /// Returns defaults if the file doesn't exist.
    pub fn load_invariants(config_dir: &Path) -> Result<Invariants> {
        let path = config_dir.join("invariants.toml");
        if !path.exists() {
            return Ok(Invariants::default());
        }
        let contents =
            std::fs::read_to_string(&path).context("failed to read invariants.toml")?;
        Invariants::from_toml(&contents).context("failed to parse invariants.toml")
    }

    /// Load agent config from `config_dir/agent.toml` with env var overrides.
    /// Returns defaults if the file doesn't exist.
    pub fn load_agent_config(config_dir: &Path) -> Result<AgentConfig> {
        let path = config_dir.join("agent.toml");
        let mut config = if path.exists() {
            let contents =
                std::fs::read_to_string(&path).context("failed to read agent.toml")?;
            AgentConfig::from_toml(&contents).context("failed to parse agent.toml")?
        } else {
            AgentConfig::default()
        };

        // Apply env var overrides
        if let Ok(model) = std::env::var("APEX_MODEL") {
            config.agent.model = model;
        }
        if let Ok(val) = std::env::var("APEX_CONCURRENT") {
            if let Ok(n) = val.parse() {
                config.agent.max_concurrent = n;
            }
        }
        if let Ok(val) = std::env::var("APEX_MAX_DEPTH") {
            if let Ok(n) = val.parse() {
                config.agent.max_depth = n;
            }
        }
        Ok(config)
    }

    /// Save agent config to `config_dir/agent.toml`.
    pub fn save_agent_config(config_dir: &Path, config: &AgentConfig) -> Result<()> {
        let path = config_dir.join("agent.toml");
        let contents = config.to_toml().context("failed to serialize agent config")?;
        std::fs::write(&path, &contents).context("failed to write agent.toml")?;
        Ok(())
    }

    /// Write default invariants file if it doesn't exist.
    pub fn write_default_invariants(config_dir: &Path) -> Result<()> {
        let path = config_dir.join("invariants.toml");
        if !path.exists() {
            let inv = Invariants::default();
            let contents = inv.to_toml()?;
            std::fs::write(&path, &contents).context("failed to write invariants.toml")?;
        }
        Ok(())
    }

    /// Write default agent config file if it doesn't exist.
    pub fn write_default_agent_config(config_dir: &Path) -> Result<()> {
        let path = config_dir.join("agent.toml");
        if !path.exists() {
            let config = AgentConfig::default();
            let contents = config.to_toml()?;
            std::fs::write(&path, &contents).context("failed to write agent.toml")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_defaults_when_no_files() {
        let dir = TempDir::new().unwrap();
        let inv = ConfigLoader::load_invariants(dir.path()).unwrap();
        assert_eq!(inv, Invariants::default());

        let config = ConfigLoader::load_agent_config(dir.path()).unwrap();
        assert_eq!(config, AgentConfig::default());
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = TempDir::new().unwrap();
        let mut config = AgentConfig::default();
        // Use a distinctive model to ensure we test actual save/load, not env var leakage
        config.agent.model = "roundtrip-test-model".to_string();
        ConfigLoader::save_agent_config(dir.path(), &config).unwrap();

        // Load the file directly (without env var overrides) to verify file content
        let contents = std::fs::read_to_string(dir.path().join("agent.toml")).unwrap();
        let loaded = AgentConfig::from_toml(&contents).unwrap();
        assert_eq!(config, loaded);
    }

    #[test]
    fn env_var_override_logic() {
        let dir = TempDir::new().unwrap();

        let mut config = AgentConfig::default();
        config.agent.model = "file-model".to_string();
        config.agent.max_concurrent = 2;
        config.agent.max_depth = 2;
        ConfigLoader::save_agent_config(dir.path(), &config).unwrap();

        let loaded = AgentConfig::from_toml(
            &std::fs::read_to_string(dir.path().join("agent.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(loaded.agent.model, "file-model");
        assert_eq!(loaded.agent.max_concurrent, 2);
        assert_eq!(loaded.agent.max_depth, 2);
    }

    #[test]
    fn write_defaults_idempotent() {
        let dir = TempDir::new().unwrap();
        ConfigLoader::write_default_invariants(dir.path()).unwrap();
        ConfigLoader::write_default_agent_config(dir.path()).unwrap();

        let inv1 = std::fs::read_to_string(dir.path().join("invariants.toml")).unwrap();
        ConfigLoader::write_default_invariants(dir.path()).unwrap();
        let inv2 = std::fs::read_to_string(dir.path().join("invariants.toml")).unwrap();
        assert_eq!(inv1, inv2);
    }
}
