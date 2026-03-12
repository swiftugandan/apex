use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use apex_core::domain::{
    list_skill_resources, slugify, LoadedSkill, Skill, SkillId, SkillStatus, ToolCall, ToolDef,
    ToolResult, ToolSchema,
};
use apex_core::error::ToolError;
use apex_core::ports::{SkillStore, ToolRegistry};

/// Tool registry for lazy-loaded first-class skills.
///
/// Three tools:
/// - `list_skills`: Returns manifests only (name, version, description, fitness). No body loaded.
/// - `use_skill`: Loads a specific skill by name, returns full approach + resources.
/// - `store_skill`: Creates or updates a skill.
pub struct SkillToolRegistry {
    skill_store: Arc<dyn SkillStore>,
    active_skills: Mutex<Vec<LoadedSkill>>,
}

impl SkillToolRegistry {
    pub fn new(skill_store: Arc<dyn SkillStore>) -> Self {
        Self {
            skill_store,
            active_skills: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ToolRegistry for SkillToolRegistry {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                schema: ToolSchema {
                    name: "list_skills".into(),
                    description: "List all available skills. Returns lightweight manifests \
                        (name, version) without loading full skill bodies. Use this for discovery."
                        .into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {},
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "use_skill".into(),
                    description: "Load a skill by name. Returns the full approach, resources, \
                        and metadata. The skill is registered as active for this session."
                        .into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Skill name (slug format, e.g. 'code-review')"
                            },
                            "version": {
                                "type": "string",
                                "description": "Skill version (default: 'latest')"
                            }
                        },
                        "required": ["name"]
                    }),
                },
            },
            ToolDef {
                schema: ToolSchema {
                    name: "store_skill".into(),
                    description: "Store or update a skill (successful approach for a task). \
                        If a skill with the same task_pattern exists, it is updated."
                        .into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Slug name for the skill (e.g. 'install-package'). Derived from task_pattern if omitted."
                            },
                            "description": {
                                "type": "string",
                                "description": "Human-readable one-liner describing the skill. Defaults to task_pattern if omitted."
                            },
                            "task_pattern": {
                                "type": "string",
                                "description": "Pattern describing what kind of task this skill applies to"
                            },
                            "approach": {
                                "type": "string",
                                "description": "Description of the approach/strategy used"
                            },
                            "tools_used": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "List of tools used in this approach"
                            },
                            "version": {
                                "type": "string",
                                "description": "Skill version (default: '1.0.0')"
                            }
                        },
                        "required": ["task_pattern", "approach"]
                    }),
                },
            },
        ]
    }

    async fn execute(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        match call.name.as_str() {
            "list_skills" => self.exec_list_skills(call).await,
            "use_skill" => self.exec_use_skill(call).await,
            "store_skill" => self.exec_store_skill(call).await,
            _ => Err(ToolError::UnknownTool(call.name.clone())),
        }
    }
}

impl SkillToolRegistry {
    async fn exec_list_skills(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let manifests = self
            .skill_store
            .list_manifests()
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        // For each manifest, load the skill to get fitness/description
        let mut entries: Vec<Value> = Vec::new();
        for m in &manifests {
            let skill = self
                .skill_store
                .load_skill(&m.name, "latest")
                .await
                .ok()
                .flatten();
            entries.push(json!({
                "name": m.name,
                "version": m.version,
                "description": skill.as_ref().map(|s| s.description.as_str()).unwrap_or(""),
                "fitness": skill.as_ref().map(|s| format!("{:.2}", s.fitness)).unwrap_or_default(),
                "status": skill.as_ref().map(|s| s.status.to_string()).unwrap_or_default(),
            }));
        }

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "count": entries.len(), "skills": entries }),
            is_error: false,
            ..Default::default()
        })
    }

    async fn exec_use_skill(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let name = call.input["name"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing name".into()))?;
        let version = call
            .input
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("latest");

        let skill = self
            .skill_store
            .load_skill(name, version)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?
            .ok_or_else(|| {
                ToolError::Execution(format!("skill '{}' v{} not found", name, version))
            })?;

        // Register as active (dedup by name+version)
        {
            let mut active = self.active_skills.lock().await;
            let already_active = active
                .iter()
                .any(|l| l.manifest.name == skill.name && l.manifest.version == skill.version);
            if !already_active {
                active.push(LoadedSkill {
                    manifest: skill.to_manifest(),
                    skill: skill.clone(),
                });
            }
        }

        let resources = skill
            .skill_dir
            .as_ref()
            .map(|dir| list_skill_resources(dir))
            .unwrap_or_default();

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({
                "name": skill.name,
                "version": skill.version,
                "description": skill.description,
                "approach": skill.approach,
                "tools_used": skill.tools_used,
                "fitness": format!("{:.2}", skill.fitness),
                "resources": resources,
            }),
            is_error: false,
            ..Default::default()
        })
    }

    async fn exec_store_skill(&self, call: &ToolCall) -> Result<ToolResult, ToolError> {
        let task_pattern = call.input["task_pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing task_pattern".into()))?;
        let approach = call.input["approach"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing approach".into()))?;
        let tools_used: Vec<String> = call
            .input
            .get("tools_used")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let name = call
            .input
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| slugify(task_pattern));
        let description = call
            .input
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| task_pattern.to_string());
        let version = call
            .input
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| "1.0.0".to_string());

        let skill = Skill {
            id: SkillId(String::new()),
            name,
            description,
            license: None,
            compatibility: None,
            allowed_tools: None,
            extra_metadata: Default::default(),
            task_pattern: task_pattern.to_string(),
            approach: approach.to_string(),
            tools_used,
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            min_samples: 3,
            last_used: String::new(),
            status: SkillStatus::Active,
            version,
            skill_dir: None,
        };

        let id = self
            .skill_store
            .store_skill(skill)
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        Ok(ToolResult {
            tool_use_id: call.id.clone(),
            name: call.name.clone(),
            output: json!({ "id": id.0, "status": "stored" }),
            is_error: false,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_core::domain::SkillManifest;
    use apex_core::error::MemoryError;

    struct MockSkillStore {
        skills: Mutex<Vec<Skill>>,
    }

    impl MockSkillStore {
        fn new() -> Self {
            Self {
                skills: Mutex::new(Vec::new()),
            }
        }

        fn with_skills(skills: Vec<Skill>) -> Self {
            Self {
                skills: Mutex::new(skills),
            }
        }
    }

    #[async_trait]
    impl SkillStore for MockSkillStore {
        async fn list_manifests(&self) -> Result<Vec<SkillManifest>, MemoryError> {
            let skills = self.skills.lock().await;
            Ok(skills.iter().map(|s| s.to_manifest()).collect())
        }
        async fn load_skill(
            &self,
            name: &str,
            version: &str,
        ) -> Result<Option<Skill>, MemoryError> {
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
                SkillId(format!("skill-{}", self.skills.lock().await.len()))
            } else {
                skill.id.clone()
            };
            self.skills.lock().await.push(Skill {
                id: id.clone(),
                ..skill
            });
            Ok(id)
        }
        async fn update_skill_fitness(
            &self,
            _id: &SkillId,
            _success: bool,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    fn make_skill(name: &str, desc: &str) -> Skill {
        Skill {
            id: SkillId(format!("skill-{name}")),
            name: name.to_string(),
            description: desc.to_string(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            extra_metadata: Default::default(),
            task_pattern: desc.to_string(),
            approach: "Test approach".to_string(),
            tools_used: vec!["shell_exec".to_string()],
            success_count: 3,
            failure_count: 1,
            fitness: 0.75,
            min_samples: 3,
            last_used: String::new(),
            status: SkillStatus::Active,
            version: "1.0.0".to_string(),
            skill_dir: None,
        }
    }

    fn setup_with_skills(skills: Vec<Skill>) -> SkillToolRegistry {
        let store: Arc<dyn SkillStore> = Arc::new(MockSkillStore::with_skills(skills));
        SkillToolRegistry::new(store)
    }

    fn setup_empty() -> SkillToolRegistry {
        let store: Arc<dyn SkillStore> = Arc::new(MockSkillStore::new());
        SkillToolRegistry::new(store)
    }

    #[test]
    fn definitions_returns_3_tools() {
        let reg = setup_empty();
        let defs = reg.definitions();
        assert_eq!(defs.len(), 3);
        let names: Vec<&str> = defs.iter().map(|d| d.schema.name.as_str()).collect();
        assert!(names.contains(&"list_skills"));
        assert!(names.contains(&"use_skill"));
        assert!(names.contains(&"store_skill"));
    }

    #[tokio::test]
    async fn list_skills_returns_manifests() {
        let reg = setup_with_skills(vec![
            make_skill("code-review", "Review code for quality"),
            make_skill("test-writing", "Write tests for code"),
        ]);

        let call = ToolCall {
            id: "t1".into(),
            name: "list_skills".into(),
            input: json!({}),
        };
        let result = reg.execute(&call).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output["count"], 2);
        let skills = result.output["skills"].as_array().unwrap();
        assert_eq!(skills[0]["name"], "code-review");
        assert_eq!(skills[1]["name"], "test-writing");
    }

    #[tokio::test]
    async fn use_skill_loads_and_activates() {
        let reg = setup_with_skills(vec![make_skill("deploy-app", "Deploy application")]);

        let call = ToolCall {
            id: "t1".into(),
            name: "use_skill".into(),
            input: json!({ "name": "deploy-app" }),
        };
        let result = reg.execute(&call).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output["name"], "deploy-app");
        assert_eq!(result.output["approach"], "Test approach");

        // Check it was registered as active
        let active = reg.active_skills.lock().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].manifest.name, "deploy-app");
    }

    #[tokio::test]
    async fn use_skill_not_found_errors() {
        let reg = setup_empty();
        let call = ToolCall {
            id: "t1".into(),
            name: "use_skill".into(),
            input: json!({ "name": "nonexistent" }),
        };
        let err = reg.execute(&call).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref msg) if msg.contains("not found")));
    }

    #[tokio::test]
    async fn store_skill_creates_new() {
        let reg = setup_empty();
        let call = ToolCall {
            id: "t1".into(),
            name: "store_skill".into(),
            input: json!({
                "task_pattern": "deploy application",
                "approach": "Run deploy script",
                "tools_used": ["shell_exec"]
            }),
        };
        let result = reg.execute(&call).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.output["status"], "stored");
    }
}
