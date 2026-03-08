use async_trait::async_trait;

use crate::domain::{
    CompletionRequest, CompletionResponse, ToolCall, ToolCompletionResponse, ToolDef, ToolResult,
    ToolSchema,
};
use crate::error::{LlmError, ToolError};

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
