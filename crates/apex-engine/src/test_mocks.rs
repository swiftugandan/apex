#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use apex_core::domain::{
    CalibrationData, ChatMessage, ClaimedTask, CompletionRequest, CompletionResponse, ContentBlock,
    Fact, FactId, MessageRole, QueueDepth, QueueMessage, QueueMessageMeta, ReapResult, Scratchpad,
    Skill, SkillId, SkillManifest, StopReason, TokenUsage, ToolCall, ToolCompletionResponse,
    ToolDef, ToolResult, ToolSchema,
};
use apex_core::error::{LlmError, MemoryError, QueueError, ToolError};
use apex_core::ports::{LlmProvider, MemoryStore, Queue, SkillStore, ToolRegistry, WorkingMemory};

use crate::claim_tool_factory::{ClaimContext, ClaimToolFactory};

// ── MockClaimToolFactory ──────────────────────────────────────────

/// Returns an empty tool registry. Sufficient for tests that never
/// process a claim (e.g. `worker_exits_on_empty_queue`).
pub struct MockClaimToolFactory;

#[async_trait]
impl ClaimToolFactory for MockClaimToolFactory {
    async fn build(&self, _ctx: &ClaimContext) -> Box<dyn ToolRegistry> {
        Box::new(MockToolRegistry::echo("unused"))
    }
}

// ── Call recording helpers ────────────────────────────────────────

pub type CallLog<T> = Arc<Mutex<Vec<T>>>;

pub fn call_log<T>() -> CallLog<T> {
    Arc::new(Mutex::new(Vec::new()))
}

// ── MockLlmProvider ───────────────────────────────────────────────

/// Scripted LLM that returns pre-programmed responses in order.
/// Panics if more calls are made than responses are queued.
pub struct MockLlmProvider {
    pub responses: Mutex<VecDeque<Result<ToolCompletionResponse, LlmError>>>,
    pub context_window: usize,
}

impl MockLlmProvider {
    pub fn new(responses: Vec<Result<ToolCompletionResponse, LlmError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            context_window: 200_000,
        }
    }

    /// Create a provider with a custom context window size.
    pub fn with_context_window(mut self, size: usize) -> Self {
        self.context_window = size;
        self
    }

    /// Create a provider that returns a single text-only response (end_turn).
    pub fn text_only(text: &str) -> Self {
        Self::new(vec![Ok(ToolCompletionResponse {
            message: ChatMessage {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
            },
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
            },
            stop_reason: StopReason::EndTurn,
        })])
    }

    /// Create a provider that returns an error.
    pub fn error(msg: &str) -> Self {
        Self::new(vec![Err(LlmError::Api(msg.to_string()))])
    }

    /// Create a provider that returns a tool call followed by a text response.
    pub fn tool_then_text(tool_call: ToolCall, final_text: &str) -> Self {
        Self::multi_tool_then_text(vec![tool_call], final_text)
    }

    /// Create a provider that returns multiple tool calls in one turn, then text.
    pub fn multi_tool_then_text(calls: Vec<ToolCall>, final_text: &str) -> Self {
        let text = final_text.to_string();
        let content: Vec<ContentBlock> = calls
            .iter()
            .map(|c| ContentBlock::ToolUse {
                id: c.id.clone(),
                name: c.name.clone(),
                input: c.input.clone(),
            })
            .collect();
        Self::new(vec![
            Ok(ToolCompletionResponse {
                message: ChatMessage {
                    role: MessageRole::Assistant,
                    content,
                },
                tool_calls: calls,
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                },
                stop_reason: StopReason::ToolUse,
            }),
            Ok(ToolCompletionResponse {
                message: ChatMessage {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text { text }],
                },
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 150,
                    output_tokens: 30,
                },
                stop_reason: StopReason::EndTurn,
            }),
        ])
    }

    /// Create a provider that always returns tool calls (for max turns testing).
    pub fn always_tool_call(call: ToolCall, count: usize) -> Self {
        Self::always_multi_tool_calls(vec![call], count)
    }

    /// Create a provider that always returns multiple tool calls per turn (for budget testing).
    pub fn always_multi_tool_calls(calls: Vec<ToolCall>, turns: usize) -> Self {
        let responses = (0..turns)
            .map(|_| {
                let content: Vec<ContentBlock> = calls
                    .iter()
                    .map(|c| ContentBlock::ToolUse {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        input: c.input.clone(),
                    })
                    .collect();
                Ok(ToolCompletionResponse {
                    message: ChatMessage {
                        role: MessageRole::Assistant,
                        content,
                    },
                    tool_calls: calls.clone(),
                    usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                    },
                    stop_reason: StopReason::ToolUse,
                })
            })
            .collect();
        Self::new(responses)
    }
}

#[async_trait]
impl LlmProvider for MockLlmProvider {
    async fn complete(&self, _req: CompletionRequest<'_>) -> Result<CompletionResponse, LlmError> {
        let mut queue = self.responses.lock().await;
        let resp = queue
            .pop_front()
            .expect("MockLlmProvider: no more responses queued");
        match resp {
            Ok(tcr) => Ok(CompletionResponse {
                message: tcr.message,
                usage: tcr.usage,
                stop_reason: tcr.stop_reason,
            }),
            Err(e) => Err(LlmError::Api(e.to_string())),
        }
    }

    async fn complete_with_tools(
        &self,
        _req: CompletionRequest<'_>,
        _tools: &[ToolSchema],
    ) -> Result<ToolCompletionResponse, LlmError> {
        let mut queue = self.responses.lock().await;
        queue
            .pop_front()
            .expect("MockLlmProvider: no more responses queued")
    }

    fn model_id(&self) -> &str {
        "mock-model"
    }

    fn context_window(&self) -> usize {
        self.context_window
    }
}

// ── MockToolRegistry ──────────────────────────────────────────────

/// Simple tool registry that records calls and returns scripted results.
pub struct MockToolRegistry {
    defs: Vec<ToolDef>,
    responses: Mutex<VecDeque<Result<ToolResult, ToolError>>>,
    pub calls: CallLog<ToolCall>,
}

impl MockToolRegistry {
    pub fn new(defs: Vec<ToolDef>, responses: Vec<Result<ToolResult, ToolError>>) -> Self {
        Self {
            defs,
            responses: Mutex::new(responses.into()),
            calls: call_log(),
        }
    }

    /// Create a registry with a single tool that always succeeds.
    pub fn echo(name: &str) -> Self {
        let def = ToolDef {
            schema: ToolSchema {
                name: name.to_string(),
                description: "mock tool".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        };
        // Queue up many success responses
        let responses: Vec<_> = (0..100)
            .map(|_| {
                Ok(ToolResult {
                    tool_use_id: String::new(),
                    name: name.to_string(),
                    output: serde_json::json!({"result": "ok"}),
                    is_error: false,
                    ..Default::default()
                })
            })
            .collect();
        Self::new(vec![def], responses)
    }

    /// Create a registry with a single tool that returns an error.
    pub fn failing(name: &str, error_msg: &str) -> Self {
        let def = ToolDef {
            schema: ToolSchema {
                name: name.to_string(),
                description: "mock tool".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        };
        let responses = vec![Err(ToolError::Execution(error_msg.to_string()))];
        Self::new(vec![def], responses)
    }

    /// Create a registry that returns a large output.
    pub fn large_output(name: &str, size_bytes: usize) -> Self {
        let def = ToolDef {
            schema: ToolSchema {
                name: name.to_string(),
                description: "mock tool".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        };
        let large = "x".repeat(size_bytes);
        let responses = vec![Ok(ToolResult {
            tool_use_id: String::new(),
            name: name.to_string(),
            output: serde_json::json!({"data": large}),
            is_error: false,
            ..Default::default()
        })];
        Self::new(vec![def], responses)
    }
}

#[async_trait]
impl ToolRegistry for MockToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        self.calls.lock().await.push(call.clone());
        let mut queue = self.responses.lock().await;
        if let Some(resp) = queue.pop_front() {
            match resp {
                Ok(mut result) => {
                    result.tool_use_id = call.id.clone();
                    Ok(result)
                }
                Err(e) => Err(e),
            }
        } else {
            Ok(ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                output: serde_json::json!({"result": "ok"}),
                is_error: false,
                ..Default::default()
            })
        }
    }
}

// ── MockQueue ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum QueueAction {
    Push(QueueMessage),
    Ack(String),
    Nack(String),
    NackWithDelay(String, Duration),
    Reject(String),
    UpdateBody(String, String),
}

pub struct MockQueue {
    pub pending: Mutex<VecDeque<ClaimedTask>>,
    pub done: Mutex<Vec<(String, String)>>,
    pub actions: CallLog<QueueAction>,
    pub depth_override: Mutex<Option<QueueDepth>>,
}

impl MockQueue {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            done: Mutex::new(Vec::new()),
            actions: call_log(),
            depth_override: Mutex::new(None),
        }
    }

    pub fn with_tasks(tasks: Vec<ClaimedTask>) -> Self {
        Self {
            pending: Mutex::new(tasks.into()),
            done: Mutex::new(Vec::new()),
            actions: call_log(),
            depth_override: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Queue for MockQueue {
    async fn push(&self, msg: QueueMessage) -> Result<String, QueueError> {
        let id = apex_core::generate_id("mock");
        self.actions.lock().await.push(QueueAction::Push(msg));
        Ok(id)
    }

    async fn pop(&self) -> Result<Option<ClaimedTask>, QueueError> {
        Ok(self.pending.lock().await.pop_front())
    }

    async fn update_body(&self, claimed: &ClaimedTask, new_body: &str) -> Result<(), QueueError> {
        self.actions.lock().await.push(QueueAction::UpdateBody(
            claimed.id.clone(),
            new_body.to_string(),
        ));
        Ok(())
    }

    async fn ack(&self, claimed: &ClaimedTask) -> Result<(), QueueError> {
        self.actions
            .lock()
            .await
            .push(QueueAction::Ack(claimed.id.clone()));
        Ok(())
    }

    async fn nack(&self, claimed: &ClaimedTask) -> Result<(), QueueError> {
        self.actions
            .lock()
            .await
            .push(QueueAction::Nack(claimed.id.clone()));
        Ok(())
    }

    async fn nack_with_delay(
        &self,
        claimed: &ClaimedTask,
        delay: Duration,
    ) -> Result<(), QueueError> {
        self.actions
            .lock()
            .await
            .push(QueueAction::NackWithDelay(claimed.id.clone(), delay));
        Ok(())
    }

    async fn reject(&self, claimed: &ClaimedTask) -> Result<(), QueueError> {
        self.actions
            .lock()
            .await
            .push(QueueAction::Reject(claimed.id.clone()));
        Ok(())
    }

    async fn depth(&self) -> Result<QueueDepth, QueueError> {
        let ovr = self.depth_override.lock().await;
        if let Some(d) = ovr.as_ref() {
            return Ok(d.clone());
        }
        let pending = self.pending.lock().await.len() as u32;
        Ok(QueueDepth {
            pending,
            processing: 0,
        })
    }

    async fn reap(&self) -> Result<ReapResult, QueueError> {
        Ok(ReapResult::default())
    }

    async fn list_done(&self, correlation_id: &str) -> Result<Vec<String>, QueueError> {
        let done = self.done.lock().await;
        Ok(done
            .iter()
            .filter(|(id, _)| id.starts_with(correlation_id) || correlation_id.is_empty())
            .map(|(id, _)| id.clone())
            .collect())
    }

    async fn read_done_body(&self, id: &str) -> Result<String, QueueError> {
        let done = self.done.lock().await;
        done.iter()
            .find(|(did, _)| did == id)
            .map(|(_, body)| body.clone())
            .ok_or_else(|| QueueError::NotFound(id.to_string()))
    }

    async fn list_with_state(&self, _state: &str) -> Result<Vec<QueueMessageMeta>, QueueError> {
        Ok(Vec::new())
    }
}

// ── MockWorkingMemory ─────────────────────────────────────────────

pub struct MockWorkingMemory {
    pub pads: Mutex<std::collections::HashMap<String, Scratchpad>>,
}

impl MockWorkingMemory {
    pub fn new() -> Self {
        Self {
            pads: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[async_trait]
impl WorkingMemory for MockWorkingMemory {
    async fn load_or_create(&self, job_id: &str) -> Result<Scratchpad, MemoryError> {
        let pads = self.pads.lock().await;
        Ok(pads
            .get(job_id)
            .cloned()
            .unwrap_or_else(|| Scratchpad::new(job_id, "")))
    }

    async fn save(&self, scratchpad: &Scratchpad) -> Result<(), MemoryError> {
        let mut pads = self.pads.lock().await;
        pads.insert(scratchpad.job_id.clone(), scratchpad.clone());
        Ok(())
    }

    async fn exists(&self, job_id: &str) -> Result<bool, MemoryError> {
        Ok(self.pads.lock().await.contains_key(job_id))
    }

    async fn delete(&self, job_id: &str) -> Result<(), MemoryError> {
        self.pads.lock().await.remove(job_id);
        Ok(())
    }

    async fn list_active(&self) -> Result<Vec<String>, MemoryError> {
        Ok(self.pads.lock().await.keys().cloned().collect())
    }

    async fn reap_stale(&self, _retention_days: u32) -> Result<Vec<String>, MemoryError> {
        Ok(Vec::new())
    }
}

// ── MockMemoryStore ───────────────────────────────────────────────

pub struct MockMemoryStore {
    pub facts: Mutex<Vec<Fact>>,
    pub calibration: Mutex<CalibrationData>,
}

impl MockMemoryStore {
    pub fn new() -> Self {
        Self {
            facts: Mutex::new(Vec::new()),
            calibration: Mutex::new(CalibrationData::default()),
        }
    }
}

#[async_trait]
impl MemoryStore for MockMemoryStore {
    async fn store_fact(&self, fact: Fact) -> Result<FactId, MemoryError> {
        let id = if fact.id.0.is_empty() {
            FactId(apex_core::generate_id("fact"))
        } else {
            fact.id.clone()
        };
        let mut facts = self.facts.lock().await;
        facts.push(Fact {
            id: id.clone(),
            ..fact
        });
        Ok(id)
    }

    async fn query_facts(&self, query: &str, limit: usize) -> Result<Vec<Fact>, MemoryError> {
        let facts = self.facts.lock().await;
        if query.is_empty() {
            return Ok(facts.iter().take(limit).cloned().collect());
        }
        Ok(facts
            .iter()
            .filter(|f| f.content.contains(query))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn verify_fact(&self, _id: &FactId) -> Result<(), MemoryError> {
        Ok(())
    }

    async fn persist_calibration(&self, data: &CalibrationData) -> Result<(), MemoryError> {
        let mut cal = self.calibration.lock().await;
        *cal = data.clone();
        Ok(())
    }

    async fn load_calibration(&self) -> Result<CalibrationData, MemoryError> {
        Ok(self.calibration.lock().await.clone())
    }
}

// ── MockSkillStore ────────────────────────────────────────────────

pub struct MockSkillStore {
    pub skills: Mutex<Vec<Skill>>,
}

impl MockSkillStore {
    pub fn new() -> Self {
        Self {
            skills: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl SkillStore for MockSkillStore {
    async fn list_manifests(&self) -> Result<Vec<SkillManifest>, MemoryError> {
        let skills = self.skills.lock().await;
        Ok(skills.iter().map(|s| s.to_manifest()).collect())
    }

    async fn load_skill(&self, name: &str, version: &str) -> Result<Option<Skill>, MemoryError> {
        let skills = self.skills.lock().await;
        Ok(skills
            .iter()
            .find(|s| s.name == name && (version == "latest" || s.version == version))
            .cloned())
    }

    async fn validate_manifest(&self, manifest: &SkillManifest) -> Result<(), MemoryError> {
        let skills = self.skills.lock().await;
        if skills.iter().any(|s| s.name == manifest.name) {
            Ok(())
        } else {
            Err(MemoryError::NotFound(manifest.name.clone()))
        }
    }

    async fn store_skill(&self, skill: Skill) -> Result<SkillId, MemoryError> {
        let id = if skill.id.0.is_empty() {
            SkillId(apex_core::generate_id("skill"))
        } else {
            skill.id.clone()
        };
        self.skills.lock().await.push(Skill {
            id: id.clone(),
            ..skill
        });
        Ok(id)
    }

    async fn update_skill_fitness(&self, id: &SkillId, success: bool) -> Result<(), MemoryError> {
        let mut skills = self.skills.lock().await;
        if let Some(skill) = skills.iter_mut().find(|s| s.id == *id) {
            if success {
                skill.success_count += 1;
            } else {
                skill.failure_count += 1;
            }
        }
        Ok(())
    }
}
