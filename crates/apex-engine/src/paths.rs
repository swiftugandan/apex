use std::path::PathBuf;
use anyhow::{Context, Result};

const DEFAULT_HOOK_NON_RETRYABLE: &str = r#"[hook]
name = "non_retryable_errors"
event = "on_failure"
priority = 0
invariant = true
propagate = true

[action]
action_type = "script"
command = """
INPUT=$(cat)
ERR=$(echo "$INPUT" | jq -r '.error // empty' | tr '[:upper:]' '[:lower:]')
for pat in "credit balance is too low" "invalid x-api-key" "invalid api key" \
           "authentication_error" "permission_error" "not_found_error" "configuration error:"; do
  echo "$ERR" | grep -qi "$pat" && echo '{"block":"Non-retryable error"}' && exit 0
done
exit 1
"""
input = "context"
on_failure = "continue"
"#;

const DEFAULT_HOOK_RATE_LIMIT: &str = r#"[hook]
name = "rate_limit_backoff"
event = "on_failure"
priority = 10
invariant = true
propagate = true

[action]
action_type = "script"
command = """
INPUT=$(cat)
ERR=$(echo "$INPUT" | jq -r '.error // empty' | tr '[:upper:]' '[:lower:]')
RETRY=$(echo "$INPUT" | jq -r '.retry_count // 0')
if echo "$ERR" | grep -qiE 'rate_limit|429|too many requests'; then
  BACKOFF=$(( 30 * (RETRY + 1) ))
  echo "{\"backoff_secs\": $BACKOFF}"
  exit 0
fi
exit 1
"""
input = "context"
on_failure = "continue"
"#;

/// Unified project paths struct used by the engine, CLI, and sub-agents.
/// Single source of truth for all directory locations.
///
/// `root` is the workspace directory (cwd). All apex state lives under
/// `root/.apex/` so the workspace is never polluted with internal files.
#[derive(Clone)]
pub struct ProjectPaths {
    pub root: PathBuf,
    pub apex_dir: PathBuf,       // root/.apex
    pub work_queue: PathBuf,     // root/.apex/queues/work
    pub queues_dir: PathBuf,     // root/.apex/queues
    pub prompts_dir: PathBuf,    // root/.apex/prompts
    pub working_memory: PathBuf, // root/.apex/memory/working
    pub long_term_dir: PathBuf,  // root/.apex/memory/long-term
    pub skills_dir: PathBuf,     // root/.apex/memory/long-term/skills
    pub scratch_dir: PathBuf,    // root/.apex/scratch
    pub tools_dir: PathBuf,      // root/.apex/tools
    pub config_dir: PathBuf,     // root/.apex/config
    pub hooks_dir: PathBuf,      // root/.apex/hooks
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
    /// All state is nested under `root/.apex/`.
    pub fn from_root(root: PathBuf) -> Self {
        let apex_dir = root.join(".apex");
        let long_term_dir = apex_dir.join("memory").join("long-term");
        Self {
            work_queue: apex_dir.join("queues").join("work"),
            queues_dir: apex_dir.join("queues"),
            prompts_dir: apex_dir.join("prompts"),
            working_memory: apex_dir.join("memory").join("working"),
            skills_dir: long_term_dir.join("skills"),
            long_term_dir,
            scratch_dir: apex_dir.join("scratch"),
            tools_dir: apex_dir.join("tools"),
            config_dir: apex_dir.join("config"),
            hooks_dir: apex_dir.join("hooks"),
            apex_dir,
            root,
        }
    }

    /// Path to the long-term memory SQLite database.
    pub fn long_term_db(&self) -> PathBuf {
        self.long_term_dir.join("memory.db")
    }

    /// Create all required directories. Used by `apex init`.
    pub fn create_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.apex_dir)
            .context("failed to create .apex/ directory")?;
        std::fs::create_dir_all(&self.queues_dir)
            .context("failed to create .apex/queues/ directory")?;
        std::fs::create_dir_all(&self.working_memory)
            .context("failed to create .apex/memory/working/ directory")?;
        std::fs::create_dir_all(&self.long_term_dir)
            .context("failed to create .apex/memory/long-term/ directory")?;
        std::fs::create_dir_all(&self.skills_dir)
            .context("failed to create .apex/memory/long-term/skills/ directory")?;
        std::fs::create_dir_all(&self.scratch_dir)
            .context("failed to create .apex/scratch/ directory")?;
        std::fs::create_dir_all(self.tools_dir.join("custom"))
            .context("failed to create .apex/tools/custom/ directory")?;
        std::fs::create_dir_all(&self.config_dir)
            .context("failed to create .apex/config/ directory")?;
        std::fs::create_dir_all(&self.prompts_dir)
            .context("failed to create .apex/prompts/ directory")?;
        std::fs::create_dir_all(&self.hooks_dir)
            .context("failed to create .apex/hooks/ directory")?;

        // Ship default on_failure hooks
        self.create_default_hooks()?;

        Ok(())
    }

    /// Write default hook TOML files if they don't already exist.
    fn create_default_hooks(&self) -> Result<()> {
        let on_failure_dir = self.hooks_dir.join("on_failure.d");
        std::fs::create_dir_all(&on_failure_dir)
            .context("failed to create .apex/hooks/on_failure.d/ directory")?;

        let non_retryable = on_failure_dir.join("non-retryable.toml");
        if !non_retryable.exists() {
            std::fs::write(&non_retryable, DEFAULT_HOOK_NON_RETRYABLE)
                .context("failed to write non-retryable.toml")?;
        }

        let rate_limit = on_failure_dir.join("rate-limit-backoff.toml");
        if !rate_limit.exists() {
            std::fs::write(&rate_limit, DEFAULT_HOOK_RATE_LIMIT)
                .context("failed to write rate-limit-backoff.toml")?;
        }

        Ok(())
    }
}
