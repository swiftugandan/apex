use std::path::PathBuf;
use anyhow::{Context, Result};

/// Unified project paths struct used by the engine, CLI, and sub-agents.
/// Single source of truth for all directory locations.
#[derive(Clone)]
pub struct ProjectPaths {
    pub root: PathBuf,
    pub work_queue: PathBuf,     // root/queues/work
    pub queues_dir: PathBuf,     // root/queues
    pub prompts_dir: PathBuf,    // root/prompts
    pub working_memory: PathBuf, // root/memory/working
    pub long_term_dir: PathBuf,  // root/memory/long-term
    pub scratch_dir: PathBuf,    // root/scratch
    pub tools_dir: PathBuf,      // root/tools
    pub config_dir: PathBuf,     // root/config
}

impl ProjectPaths {
    /// Resolve project paths from APEX_ROOT env var or current directory.
    pub fn resolve() -> Result<Self> {
        let root = std::env::var("APEX_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Ok(Self::from_root(root))
    }

    /// Build paths from a given root directory.
    pub fn from_root(root: PathBuf) -> Self {
        Self {
            work_queue: root.join("queues").join("work"),
            queues_dir: root.join("queues"),
            prompts_dir: root.join("prompts"),
            working_memory: root.join("memory").join("working"),
            long_term_dir: root.join("memory").join("long-term"),
            scratch_dir: root.join("scratch"),
            tools_dir: root.join("tools"),
            config_dir: root.join("config"),
            root,
        }
    }

    /// Path to the long-term memory SQLite database.
    pub fn long_term_db(&self) -> PathBuf {
        self.long_term_dir.join("memory.db")
    }

    /// Create all required directories. Used by `apex init`.
    pub fn create_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.queues_dir)
            .context("failed to create queues/ directory")?;
        std::fs::create_dir_all(&self.working_memory)
            .context("failed to create memory/working/ directory")?;
        std::fs::create_dir_all(&self.long_term_dir)
            .context("failed to create memory/long-term/ directory")?;
        std::fs::create_dir_all(&self.scratch_dir)
            .context("failed to create scratch/ directory")?;
        std::fs::create_dir_all(self.tools_dir.join("custom"))
            .context("failed to create tools/custom/ directory")?;
        std::fs::create_dir_all(&self.config_dir)
            .context("failed to create config/ directory")?;
        std::fs::create_dir_all(&self.prompts_dir)
            .context("failed to create prompts/ directory")?;
        Ok(())
    }
}
