use async_trait::async_trait;

use crate::domain::{
    ClaimedTask, CompletionRequest, CompletionResponse, QueueDepth, QueueMessage, ReapResult,
    Scratchpad, ToolCall, ToolCompletionResponse, ToolDef, ToolResult, ToolSchema,
};
use crate::error::{LlmError, MemoryError, QueueError, ToolError};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, LlmError>;

    async fn complete_with_tools(
        &self,
        req: CompletionRequest,
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
}

#[async_trait]
pub trait Queue: Send + Sync {
    async fn push(&self, msg: QueueMessage) -> Result<String, QueueError>;
    async fn pop(&self) -> Result<Option<ClaimedTask>, QueueError>;
    async fn update_body(&self, claimed: &ClaimedTask, new_body: &str) -> Result<(), QueueError>;
    async fn ack(&self, claimed: &ClaimedTask) -> Result<(), QueueError>;
    async fn nack(&self, claimed: &ClaimedTask) -> Result<(), QueueError>;
    async fn depth(&self) -> Result<QueueDepth, QueueError>;
    async fn reap(&self) -> Result<ReapResult, QueueError>;
}

#[async_trait]
pub trait WorkingMemory: Send + Sync {
    async fn load_or_create(&self, job_id: &str) -> Result<Scratchpad, MemoryError>;
    async fn save(&self, scratchpad: &Scratchpad) -> Result<(), MemoryError>;
    async fn exists(&self, job_id: &str) -> Result<bool, MemoryError>;
    async fn delete(&self, job_id: &str) -> Result<(), MemoryError>;
    async fn list_active(&self) -> Result<Vec<String>, MemoryError>;
}
