use std::sync::Arc;

use async_trait::async_trait;

use apex_core::domain::{
    slugify, AttemptRecord, CacheHint, ChatMessage, CompletionRequest, ExtractedSkill, SystemBlock,
};
use apex_core::ports::{LlmProvider, SkillExtractor, SkillStore};

/// Maximum chars for the tool trace sent to the LLM for skill extraction.
const MAX_TOOL_TRACE_CHARS: usize = 4000;

/// Build a compact one-line-per-tool-call trace from attempt turns.
fn build_tool_trace(record: &AttemptRecord) -> String {
    let mut trace = String::new();
    for turn in &record.turns {
        for tc in &turn.tool_calls {
            let line = format!("{}({})\n", tc.name, tc.input_summary);
            if trace.len() + line.len() > MAX_TOOL_TRACE_CHARS {
                trace.push_str("...(truncated)\n");
                return trace;
            }
            trace.push_str(&line);
        }
    }
    trace
}

pub struct LlmSkillExtractor {
    llm: Arc<dyn LlmProvider>,
}

impl LlmSkillExtractor {
    pub fn new(llm: Arc<dyn LlmProvider>) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl SkillExtractor for LlmSkillExtractor {
    async fn extract_skill(
        &self,
        goal: &str,
        record: &AttemptRecord,
        skill_store: &dyn SkillStore,
    ) -> Option<ExtractedSkill> {
        let tool_trace = build_tool_trace(record);

        let existing_skill_names: Vec<String> = skill_store
            .list_manifests()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.name)
            .collect();

        let existing_list = if existing_skill_names.is_empty() {
            "(none)".to_string()
        } else {
            existing_skill_names.join(", ")
        };

        let prompt = format!(
            r#"You are an AI skill cataloger. Given a completed task and its tool trace, produce a reusable skill entry.

Task goal: {goal}

Tool trace:
{tool_trace}

Existing skill names (reuse one if this task matches): {existing_list}

Return ONLY a JSON object with these fields:
- "name": a generalized, lowercase-hyphenated skill name (max 64 chars). Example: "analyze-codebase-architecture". Reuse an existing name if the task is essentially the same skill.
- "description": imperative phrasing starting with "Use this skill when...", max 1024 chars.
- "approach": numbered step-by-step instructions an agent can follow to reproduce this skill. Be specific about which tools to use and in what order.

JSON only, no markdown fences."#
        );

        let messages = vec![ChatMessage::user_text(&prompt)];
        let system_blocks = [SystemBlock {
            text: "You are a concise JSON generator. Output valid JSON only.".to_string(),
            cache_hint: CacheHint::Dynamic,
        }];

        let req = CompletionRequest {
            system_blocks: &system_blocks,
            messages: &messages,
            max_tokens: 16000,
            temperature: Some(0.0),
            cache_tools: false,
            reserved_reasoning_tokens: 0,
        };

        let resp = self.llm.complete(req).await.ok()?;
        let text = resp.text();
        if text.is_empty() {
            return None;
        }

        // Parse JSON — try stripping markdown fences as safety net.
        let json_str = text
            .trim()
            .strip_prefix("```json")
            .or_else(|| text.trim().strip_prefix("```"))
            .unwrap_or(text.trim())
            .strip_suffix("```")
            .unwrap_or(text.trim())
            .trim();

        let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
        let name = slugify(parsed.get("name")?.as_str()?);
        let description = parsed.get("description")?.as_str()?.to_string();
        // Accept approach as either a string or an array of strings.
        let approach = match parsed.get("approach")? {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return None,
        };

        if name.is_empty() || description.is_empty() || approach.is_empty() {
            return None;
        }

        // Enforce max lengths.
        let name = if name.len() > 64 {
            name[..64].trim_end_matches('-').to_string()
        } else {
            name
        };
        let description = if description.len() > 1024 {
            description[..1024].to_string()
        } else {
            description
        };

        Some(ExtractedSkill {
            name,
            description,
            approach,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::{
        CompletionResponse, ContentBlock, MessageRole, Skill, SkillId, SkillManifest, StopReason,
        TokenUsage, ToolCallRecord, TurnRecord,
    };
    use apex_core::error::{LlmError, MemoryError};
    use async_trait::async_trait;

    // ── Mock LLM provider ───────────────────────────────────────────

    struct MockLlmProvider {
        response: Result<String, LlmError>,
    }

    impl MockLlmProvider {
        fn success(text: &str) -> Self {
            Self {
                response: Ok(text.to_string()),
            }
        }

        fn error() -> Self {
            Self {
                response: Err(LlmError::Api("mock error".into())),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockLlmProvider {
        async fn complete(
            &self,
            _req: CompletionRequest<'_>,
        ) -> Result<CompletionResponse, LlmError> {
            match &self.response {
                Ok(text) => Ok(CompletionResponse {
                    message: ChatMessage {
                        role: MessageRole::Assistant,
                        content: vec![ContentBlock::Text { text: text.clone() }],
                    },
                    usage: TokenUsage {
                        input_tokens: 100,
                        output_tokens: 50,
                        ..Default::default()
                    },
                    stop_reason: StopReason::EndTurn,
                }),
                Err(e) => Err(LlmError::Api(e.to_string())),
            }
        }

        async fn complete_with_tools(
            &self,
            _req: CompletionRequest<'_>,
            _tools: &[apex_core::domain::ToolSchema],
        ) -> Result<apex_core::domain::ToolCompletionResponse, LlmError> {
            unimplemented!("not needed for these tests")
        }

        fn model_id(&self) -> &str {
            "mock-model"
        }
        fn context_window(&self) -> usize {
            200_000
        }
    }

    // ── Mock SkillStore ─────────────────────────────────────────────

    struct MockSkillStore {
        manifests: Vec<SkillManifest>,
    }

    impl MockSkillStore {
        fn empty() -> Self {
            Self {
                manifests: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl SkillStore for MockSkillStore {
        async fn list_manifests(&self) -> Result<Vec<SkillManifest>, MemoryError> {
            Ok(self.manifests.clone())
        }
        async fn load_skill(
            &self,
            _name: &str,
            _version: &str,
        ) -> Result<Option<Skill>, MemoryError> {
            Ok(None)
        }
        async fn validate_manifest(&self, _manifest: &SkillManifest) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn store_skill(&self, _skill: Skill) -> Result<SkillId, MemoryError> {
            Ok(SkillId("mock".to_string()))
        }
        async fn update_skill_fitness(
            &self,
            _id: &SkillId,
            _success: bool,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    // ── Helper ──────────────────────────────────────────────────────

    fn make_record_with_tools(tool_names: &[&str]) -> AttemptRecord {
        let tool_calls: Vec<ToolCallRecord> = tool_names
            .iter()
            .map(|name| ToolCallRecord {
                name: name.to_string(),
                input_summary: "test input".to_string(),
                output_summary: "test output".to_string(),
                is_error: false,
                duration_ms: 100,
                error_output: None,
            })
            .collect();
        AttemptRecord {
            attempt_number: 1,
            started_at: String::new(),
            finished_at: String::new(),
            outcome: apex_core::domain::AttemptOutcome::Success,
            final_text: Some("Task completed successfully.".to_string()),
            turns: vec![TurnRecord {
                tool_calls,
                usage: TokenUsage::default(),
            }],
            failure_reason: None,
        }
    }

    // ── Skill extractor tests ───────────────────────────────────────

    #[tokio::test]
    async fn extract_skill_valid_json() {
        let llm = Arc::new(MockLlmProvider::success(
            r#"{"name": "analyze-architecture", "description": "Use this skill when analyzing codebase architecture.", "approach": "1. Read structure\n2. Map deps"}"#,
        ));
        let extractor = LlmSkillExtractor::new(llm);
        let record = make_record_with_tools(&["shell_exec", "file_read"]);
        let skill_store = MockSkillStore::empty();

        let result = extractor
            .extract_skill("Analyze the codebase", &record, &skill_store)
            .await;

        assert!(result.is_some());
        let skill = result.unwrap();
        assert_eq!(skill.name, "analyze-architecture");
        assert!(skill.description.contains("Use this skill when"));
    }

    #[tokio::test]
    async fn extract_skill_bad_json_returns_none() {
        let llm = Arc::new(MockLlmProvider::success("This is not JSON!"));
        let extractor = LlmSkillExtractor::new(llm);
        let record = make_record_with_tools(&["shell_exec"]);
        let skill_store = MockSkillStore::empty();

        let result = extractor
            .extract_skill("Run tests", &record, &skill_store)
            .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn extract_skill_llm_error_returns_none() {
        let llm = Arc::new(MockLlmProvider::error());
        let extractor = LlmSkillExtractor::new(llm);
        let record = make_record_with_tools(&["file_read"]);
        let skill_store = MockSkillStore::empty();

        let result = extractor
            .extract_skill("Read config", &record, &skill_store)
            .await;

        assert!(result.is_none());
    }

    #[test]
    fn build_tool_trace_formats_correctly() {
        let record = make_record_with_tools(&["shell_exec", "file_read"]);
        let trace = build_tool_trace(&record);
        assert!(trace.contains("shell_exec(test input)"));
        assert!(trace.contains("file_read(test input)"));
    }

    #[test]
    fn build_tool_trace_truncates_at_limit() {
        let names: Vec<&str> = (0..200)
            .map(|_| "very_long_tool_name_for_testing")
            .collect();
        let record = make_record_with_tools(&names);
        let trace = build_tool_trace(&record);
        assert!(trace.len() <= MAX_TOOL_TRACE_CHARS + 50);
        assert!(trace.contains("...(truncated)"));
    }
}
