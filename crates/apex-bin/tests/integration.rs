use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use apex_core::config::{CompactionSection, ConsolidationSection};
use apex_core::context::TokenEstimator;
use apex_core::domain::{
    CalibrationData, ChatMessage, CompletionRequest, CompletionResponse, ContentBlock, Fact,
    FactId, LoopLimits, MessageRole, QueueMessage, Scratchpad, Skill, SkillId, SkillManifest,
    StopReason, TokenUsage, ToolCompletionResponse, ToolSchema,
};
use apex_core::error::{LlmError, MemoryError};
use apex_core::ports::{LlmProvider, MemoryStore, Queue, SkillStore, ToolRegistry, WorkingMemory};

use apex_engine::util::composer_from_estimator;
use apex_engine::{
    worker_loop, ClaimContext, ClaimToolFactory, CompositeToolRegistry, WorkerContext, WorkerLimits,
};
use apex_infra::RfbmqAdapter;
use apex_tools::QueueToolRegistry;

// ── Shared mock implementations for integration tests ─────────────

struct IntegrationLlm {
    responses: Mutex<VecDeque<Result<ToolCompletionResponse, LlmError>>>,
}

impl IntegrationLlm {
    fn text_only(text: &str) -> Self {
        Self {
            responses: Mutex::new(
                vec![Ok(ToolCompletionResponse {
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
                        ..Default::default()
                    },
                    stop_reason: StopReason::EndTurn,
                })]
                .into(),
            ),
        }
    }
}

#[async_trait]
impl LlmProvider for IntegrationLlm {
    async fn complete(&self, _req: CompletionRequest<'_>) -> Result<CompletionResponse, LlmError> {
        let mut q = self.responses.lock().await;
        let resp = q.pop_front().expect("no more responses");
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
        let mut q = self.responses.lock().await;
        q.pop_front().expect("no more responses")
    }

    fn model_id(&self) -> &str {
        "integration-mock"
    }
    fn context_window(&self) -> usize {
        200_000
    }
}

struct InMemoryWorkingMemory {
    pads: Mutex<std::collections::HashMap<String, Scratchpad>>,
}

impl InMemoryWorkingMemory {
    fn new() -> Self {
        Self {
            pads: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[async_trait]
impl WorkingMemory for InMemoryWorkingMemory {
    async fn load_or_create(&self, job_id: &str) -> Result<Scratchpad, MemoryError> {
        let pads = self.pads.lock().await;
        Ok(pads
            .get(job_id)
            .cloned()
            .unwrap_or_else(|| Scratchpad::new(job_id, "")))
    }
    async fn save(&self, sp: &Scratchpad) -> Result<(), MemoryError> {
        self.pads.lock().await.insert(sp.job_id.clone(), sp.clone());
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
    async fn reap_stale(&self, _days: u32) -> Result<Vec<String>, MemoryError> {
        Ok(vec![])
    }
}

struct InMemoryStore {
    facts: Mutex<Vec<Fact>>,
    calibration: Mutex<CalibrationData>,
}

impl InMemoryStore {
    fn new() -> Self {
        Self {
            facts: Mutex::new(Vec::new()),
            calibration: Mutex::new(CalibrationData::default()),
        }
    }
}

#[async_trait]
impl MemoryStore for InMemoryStore {
    async fn store_fact(&self, fact: Fact) -> Result<FactId, MemoryError> {
        let id = FactId(apex_core::generate_id("fact"));
        self.facts.lock().await.push(Fact {
            id: id.clone(),
            ..fact
        });
        Ok(id)
    }
    async fn query_facts(&self, _q: &str, _limit: usize) -> Result<Vec<Fact>, MemoryError> {
        Ok(vec![])
    }
    async fn verify_fact(&self, _id: &FactId) -> Result<(), MemoryError> {
        Ok(())
    }
    async fn persist_calibration(&self, data: &CalibrationData) -> Result<(), MemoryError> {
        *self.calibration.lock().await = data.clone();
        Ok(())
    }
    async fn load_calibration(&self) -> Result<CalibrationData, MemoryError> {
        Ok(self.calibration.lock().await.clone())
    }
}

struct InMemorySkillStore;

#[async_trait]
impl SkillStore for InMemorySkillStore {
    async fn list_manifests(&self) -> Result<Vec<SkillManifest>, MemoryError> {
        Ok(vec![])
    }
    async fn load_skill(&self, _name: &str, _version: &str) -> Result<Option<Skill>, MemoryError> {
        Ok(None)
    }
    async fn validate_manifest(&self, _manifest: &SkillManifest) -> Result<(), MemoryError> {
        Ok(())
    }
    async fn store_skill(&self, _skill: Skill) -> Result<SkillId, MemoryError> {
        Ok(SkillId(apex_core::generate_id("skill")))
    }
    async fn update_skill_fitness(&self, _id: &SkillId, _success: bool) -> Result<(), MemoryError> {
        Ok(())
    }
}

// ── IntegrationClaimToolFactory ───────────────────────────────────
//
// Lightweight factory for integration tests.  Builds a per-claim
// CompositeToolRegistry containing only queue tools (no static tools).

struct IntegrationClaimToolFactory {
    estimator: Arc<Mutex<TokenEstimator>>,
}

#[async_trait]
impl ClaimToolFactory for IntegrationClaimToolFactory {
    async fn build(&self, ctx: &ClaimContext) -> Box<dyn ToolRegistry> {
        let composer = composer_from_estimator(&self.estimator).await;
        let queue_tools = QueueToolRegistry::new(
            Arc::clone(&ctx.queue),
            ctx.correlation_id.clone(),
            ctx.current_depth,
            ctx.max_depth,
            ctx.parent_goal.clone(),
            ctx.parent_body.clone(),
            Some(Arc::clone(&ctx.long_term)),
            Some(Arc::clone(&ctx.skills)),
            composer,
        )
        .with_hooks(ctx.hooks.clone());

        Box::new(CompositeToolRegistry::new(vec![Box::new(queue_tools)]))
    }
}

// ── Integration test ──────────────────────────────────────────────

#[tokio::test]
#[ignore] // Run with `cargo test --workspace -- --ignored`
async fn single_task_roundtrip() {
    use apex_core::domain::{MessageHeaders, MessageType};

    let tmp = tempfile::tempdir().unwrap();
    let queue_dir = tmp.path().join("queue");

    // Initialize a real rfbmq queue
    let queue = RfbmqAdapter::init(&queue_dir).unwrap();
    let queue: Arc<dyn Queue> = Arc::new(queue);

    // Push a Goal message
    let msg = QueueMessage {
        headers: MessageHeaders {
            message_type: MessageType::Goal,
            correlation_id: "test-corr-001".into(),
            depth: 0,
            retry_count: 0,
            depends_on: vec![],
            skills: vec![],
        },
        body: "# Task: Say hello\n\nRespond with 'Hello, World!'".into(),
    };
    let _msg_id = queue.push(msg).await.unwrap();

    // Set up worker context with mock LLM
    let llm: Arc<dyn LlmProvider> = Arc::new(IntegrationLlm::text_only(
        "Hello, World! Task completed successfully.",
    ));
    let memory: Arc<dyn WorkingMemory> = Arc::new(InMemoryWorkingMemory::new());
    let long_term: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
    let skills: Arc<dyn SkillStore> = Arc::new(InMemorySkillStore);
    let estimator = Arc::new(Mutex::new(TokenEstimator::default()));
    let claim_factory: Arc<dyn ClaimToolFactory> = Arc::new(IntegrationClaimToolFactory {
        estimator: estimator.clone(),
    });

    let compactor: Arc<dyn apex_core::ports::ConversationCompactor> =
        Arc::new(apex_infra::LlmConversationCompactor::new(llm.clone()));
    let ctx = WorkerContext {
        queue: Arc::clone(&queue),
        claim_tool_factory: claim_factory,
        llm,
        compactor,
        skill_extractor: None,
        memory,
        long_term,
        skills,
        persona: Arc::new("You are a helpful assistant.".to_string()),
        limits: WorkerLimits {
            max_depth: 3,
            max_retries: 3,
            max_empty_cycles: 300,
            limits: LoopLimits {
                max_tool_result_bytes: 10_000,
                max_output_tokens: 4096,
                reserved_reasoning_tokens: 4096,
                max_turns: 32,
                max_tool_input_bytes: 40_000,
                max_tool_calls_per_turn: 64,
                max_total_tool_calls: 512,
                prompt_caching: true,
            },
        },
        estimator,
        compaction: CompactionSection {
            preserve_turns: 3,
            max_summary_tokens: 1024,
            spill_history: false,
        },
        consolidation: ConsolidationSection::default(),
        hooks: None,
        scratch_dir: None,
        orientation_factory: None,
    };

    // Run the worker loop — it should process the one message and exit
    let result = worker_loop(ctx, 0).await;
    assert!(result.is_ok(), "worker_loop failed: {:?}", result);

    // Verify the message landed in done/
    let depth = queue.depth().await.unwrap();
    assert_eq!(depth.pending, 0, "should have no pending messages");
    assert_eq!(depth.processing, 0, "should have no processing messages");

    // Verify done messages
    let done_ids = queue.list_done("test-corr-001").await.unwrap();
    assert_eq!(done_ids.len(), 1, "should have one done message");

    let body = queue.read_done_body(&done_ids[0]).await.unwrap();
    assert!(
        body.contains("Hello, World!"),
        "done body should contain the LLM response, got: {}",
        &body[..body.len().min(200)]
    );
}
