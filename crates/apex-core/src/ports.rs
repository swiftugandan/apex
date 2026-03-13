use async_trait::async_trait;

use crate::config::RoleProfile;
use crate::domain::{
    AttemptRecord, CalibrationData, ChatMessage, ClaimedTask, CompletionRequest,
    CompletionResponse, ExtractedSkill, Fact, FactId, HookDef, HookEvent, HookOutcome, QueueDepth,
    QueueMessage, QueueMessageMeta, ReapResult, Scratchpad, Skill, SkillId, SkillManifest,
    ToolCall, ToolCompletionResponse, ToolDef, ToolResult, ToolSchema,
};
use crate::error::{LlmError, MemoryError, QueueError, ToolError};
use serde_json::Value;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, req: CompletionRequest<'_>) -> Result<CompletionResponse, LlmError>;

    async fn complete_with_tools(
        &self,
        req: CompletionRequest<'_>,
        tools: &[ToolSchema],
    ) -> Result<ToolCompletionResponse, LlmError>;

    fn model_id(&self) -> &str;

    fn context_window(&self) -> usize;
}

#[async_trait]
pub trait ToolRegistry: Send + Sync {
    fn definitions(&self) -> Vec<ToolDef>;

    fn schemas(&self) -> Vec<ToolSchema> {
        self.definitions().into_iter().map(|d| d.schema).collect()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError>;

    /// Post-execution input rewriter. Returns `Some(rewritten)` if the tool
    /// input should be compressed in conversation history, `None` to keep as-is.
    /// Only called after successful execution; the original input is spilled
    /// to scratch before rewriting.
    fn rewrite_input(
        &self,
        _call: &ToolCall,
        _result: &ToolResult,
        _max_bytes: usize,
    ) -> Option<Value> {
        None
    }
}

#[async_trait]
pub trait Queue: Send + Sync {
    async fn push(&self, msg: QueueMessage) -> Result<String, QueueError>;
    async fn pop(&self) -> Result<Option<ClaimedTask>, QueueError>;
    async fn update_body(&self, claimed: &ClaimedTask, new_body: &str) -> Result<(), QueueError>;
    async fn ack(&self, claimed: &ClaimedTask) -> Result<(), QueueError>;
    async fn nack(&self, claimed: &ClaimedTask) -> Result<(), QueueError>;
    /// Nack with a delay: the message won't be redelivered until `delay` has elapsed.
    async fn nack_with_delay(
        &self,
        claimed: &ClaimedTask,
        delay: std::time::Duration,
    ) -> Result<(), QueueError>;
    /// Move a claimed message directly to failed/ without retrying.
    async fn reject(&self, claimed: &ClaimedTask) -> Result<(), QueueError>;
    async fn depth(&self) -> Result<QueueDepth, QueueError>;
    async fn reap(&self) -> Result<ReapResult, QueueError>;
    async fn list_done(&self, correlation_id: &str) -> Result<Vec<String>, QueueError>;
    async fn read_done_body(&self, id: &str) -> Result<String, QueueError>;
    /// List message metadata in a queue state directory (pending, processing, done, failed).
    async fn list_with_state(&self, state: &str) -> Result<Vec<QueueMessageMeta>, QueueError>;
}

#[async_trait]
pub trait WorkingMemory: Send + Sync {
    async fn load_or_create(&self, job_id: &str) -> Result<Scratchpad, MemoryError>;
    async fn save(&self, scratchpad: &Scratchpad) -> Result<(), MemoryError>;
    async fn exists(&self, job_id: &str) -> Result<bool, MemoryError>;
    async fn delete(&self, job_id: &str) -> Result<(), MemoryError>;
    async fn list_active(&self) -> Result<Vec<String>, MemoryError>;
    async fn reap_stale(&self, retention_days: u32) -> Result<Vec<String>, MemoryError>;
}

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store_fact(&self, fact: Fact) -> Result<FactId, MemoryError>;
    async fn query_facts(&self, query: &str, limit: usize) -> Result<Vec<Fact>, MemoryError>;
    async fn verify_fact(&self, id: &FactId) -> Result<(), MemoryError>;
    async fn persist_calibration(&self, data: &CalibrationData) -> Result<(), MemoryError>;
    async fn load_calibration(&self) -> Result<CalibrationData, MemoryError>;
}

#[async_trait]
pub trait SkillStore: Send + Sync {
    /// List all skill manifests (lightweight, no body loading).
    async fn list_manifests(&self) -> Result<Vec<SkillManifest>, MemoryError>;
    /// Load a specific skill by name + version. Returns full Skill with body.
    async fn load_skill(&self, name: &str, version: &str) -> Result<Option<Skill>, MemoryError>;
    /// Validate a manifest against available skills.
    async fn validate_manifest(&self, manifest: &SkillManifest) -> Result<(), MemoryError>;
    /// Store or upsert a skill (used by consolidation).
    async fn store_skill(&self, skill: Skill) -> Result<SkillId, MemoryError>;
    /// Update fitness after use.
    async fn update_skill_fitness(&self, id: &SkillId, success: bool) -> Result<(), MemoryError>;
}

/// Registry of lifecycle hooks. Loads hooks from `.apex/hooks/` directories
/// and dispatches them at lifecycle points.
#[async_trait]
pub trait HookRegistry: Send + Sync {
    /// Get all hook definitions for a given event, sorted by priority (ascending).
    fn hooks_for(&self, event: HookEvent) -> Vec<HookDef>;

    /// Get all loaded hook definitions.
    fn all_hooks(&self) -> Vec<HookDef>;

    /// Execute all hooks for a given event with the provided context data.
    /// Returns a list of outcomes. If any outcome is Block, the caller should
    /// prevent the event from proceeding.
    async fn dispatch(&self, event: HookEvent, context: &serde_json::Value) -> Vec<HookOutcome>;

    /// Reload hooks from disk.
    fn reload(&mut self) -> Result<(), String>;

    /// Check if any hooks are registered for the given event (without reloading).
    /// Used to skip dispatch for high-frequency events like OnLog.
    fn has_hooks_for(&self, event: HookEvent) -> bool {
        !self.hooks_for(event).is_empty()
    }
}

/// Result returned after a sub-agent completes.
pub struct SubAgentResult {
    pub done_bodies: Vec<String>,
    pub failed_bodies: Vec<String>,
}

/// Summarize older messages in a conversation to reduce token count.
#[async_trait]
pub trait ConversationCompactor: Send + Sync {
    /// Summarize older messages, preserving the first message and the last
    /// `preserve_turns` turns. Returns (compacted_messages, count_summarized).
    async fn compact(
        &self,
        messages: &[ChatMessage],
        preserve_turns: usize,
        max_summary_tokens: u32,
    ) -> Result<(Vec<ChatMessage>, usize), String>;
}

/// Extract a reusable skill from a completed task attempt.
#[async_trait]
pub trait SkillExtractor: Send + Sync {
    /// Extract a reusable skill from a completed attempt.
    /// Returns None on failure (caller uses deterministic fallback).
    async fn extract_skill(
        &self,
        goal: &str,
        record: &AttemptRecord,
        skill_store: &dyn SkillStore,
    ) -> Option<ExtractedSkill>;
}

/// Trait for spawning sub-agent processes. Decouples the delegate tool from
/// concrete queue/memory/LLM provisioning.
#[async_trait]
pub trait SubAgentSpawner: Send + Sync {
    async fn spawn(
        &self,
        task: &str,
        role: &RoleProfile,
        persona: &str,
    ) -> Result<SubAgentResult, ToolError>;
}
