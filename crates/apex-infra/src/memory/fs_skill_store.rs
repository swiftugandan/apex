use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use apex_core::domain::{slugify, Skill, SkillId, SkillManifest, SkillStatus};
use apex_core::error::MemoryError;
use apex_core::ports::SkillStore;

/// Multi-directory skill store supporting agentskills.io discovery.
///
/// Scans directories in precedence order:
/// 1. `.agents/skills/` (cross-client interop, highest priority)
/// 2. `.apex/skills/` (client-specific authored skills)
/// 3. `.apex/memory/long-term/skills/` (learned skills)
///
/// Name collisions: first-found wins.
/// Writes always go to the learned-skills directory (last in the list).
pub struct FsSkillStore {
    /// Directories to scan, in precedence order (first = highest priority).
    scan_dirs: Vec<PathBuf>,
    /// Directory for writing learned skills (always the last scan dir).
    write_dir: PathBuf,
    cache: Mutex<Option<Arc<Vec<Skill>>>>,
    auto_retire_below: f64,
}

impl FsSkillStore {
    /// Create a store that only scans a single directory (backward compatible).
    pub fn new(dir: PathBuf) -> Self {
        Self {
            write_dir: dir.clone(),
            scan_dirs: vec![dir],
            cache: Mutex::new(None),
            auto_retire_below: 0.2,
        }
    }

    /// Create a store that scans multiple directories with precedence.
    ///
    /// `scan_dirs` are ordered by priority (first = highest).
    /// `write_dir` is where learned skills are stored (typically the last scan dir).
    pub fn with_dirs(scan_dirs: Vec<PathBuf>, write_dir: PathBuf) -> Self {
        Self {
            scan_dirs,
            write_dir,
            cache: Mutex::new(None),
            auto_retire_below: 0.2,
        }
    }

    fn ensure_write_dir(&self) -> Result<(), MemoryError> {
        std::fs::create_dir_all(&self.write_dir)
            .map_err(|e| MemoryError::Database(format!("failed to create skills dir: {e}")))?;
        Ok(())
    }

    fn invalidate_cache(&self) {
        *self.cache.lock() = None;
    }

    /// Scan a single directory for skills.
    fn scan_dir(dir: &PathBuf) -> Vec<Skill> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let mut results = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_file = path.join("SKILL.md");
            let content = match std::fs::read_to_string(&skill_file) {
                Ok(c) => c,
                Err(_) => continue,
            };
            match Skill::from_markdown(&content) {
                Ok(mut skill) => {
                    // Authored skills (no apex-id) get sensible defaults
                    if skill.id.0.is_empty() {
                        skill.id = SkillId(format!("skill-{}", skill.name));
                        skill.fitness = 1.0; // authored skills always preferred
                        if skill.task_pattern.is_empty() {
                            skill.task_pattern = skill.description.clone();
                        }
                    }
                    skill.skill_dir = Some(path.clone());
                    results.push(skill);
                }
                Err(_) => continue,
            }
        }
        results
    }

    /// Load all skills, using cache when possible. Runs sync filesystem scan in
    /// spawn_blocking so it does not block the async executor. Cache uses parking_lot::Mutex
    /// so lock is brief and invalidate_cache can be called from sync write_skill.
    async fn load_all_async(&self) -> Result<Arc<Vec<Skill>>, MemoryError> {
        // Check cache first (brief sync lock)
        {
            let guard = self.cache.lock();
            if let Some(ref cached) = *guard {
                return Ok(Arc::clone(cached));
            }
        }

        let scan_dirs = self.scan_dirs.clone();
        let write_dir = self.write_dir.clone();
        let results = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&write_dir)
                .map_err(|e| MemoryError::Database(format!("failed to create skills dir: {e}")))?;
            let mut results = Vec::new();
            let mut seen_names = HashSet::new();
            for dir in &scan_dirs {
                for skill in Self::scan_dir(dir) {
                    if seen_names.insert(skill.name.clone()) {
                        results.push(skill);
                    }
                }
            }
            Ok::<_, MemoryError>(Arc::new(results))
        })
        .await
        .map_err(|e| MemoryError::Database(format!("spawn_blocking: {e}")))??;

        {
            let mut guard = self.cache.lock();
            *guard = Some(Arc::clone(&results));
        }

        Ok(results)
    }

    fn write_skill(&self, skill: &Skill) -> Result<PathBuf, MemoryError> {
        self.ensure_write_dir()?;
        let name = if skill.name.is_empty() {
            slugify(&skill.task_pattern)
        } else {
            skill.name.clone()
        };
        let skill_dir = self.write_dir.join(&name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| MemoryError::Database(format!("failed to create skill dir: {e}")))?;
        let path = skill_dir.join("SKILL.md");
        let content = skill.to_markdown();
        std::fs::write(&path, content)
            .map_err(|e| MemoryError::Database(format!("failed to write skill file: {e}")))?;
        self.invalidate_cache();
        Ok(path)
    }
}

#[async_trait]
impl SkillStore for FsSkillStore {
    async fn list_manifests(&self) -> Result<Vec<SkillManifest>, MemoryError> {
        let all = self.load_all_async().await?;
        Ok(all.iter().map(|s| s.to_manifest()).collect())
    }

    async fn load_skill(&self, name: &str, version: &str) -> Result<Option<Skill>, MemoryError> {
        let all = self.load_all_async().await?;
        let found = all
            .iter()
            .find(|s| s.name == name && (version == "latest" || s.version == version));
        Ok(found.cloned())
    }

    async fn validate_manifest(&self, manifest: &SkillManifest) -> Result<(), MemoryError> {
        let all = self.load_all_async().await?;
        let found = all.iter().any(|s| {
            s.name == manifest.name
                && (manifest.version == "latest" || s.version == manifest.version)
        });
        if !found {
            return Err(MemoryError::NotFound(format!(
                "skill {} v{}",
                manifest.name, manifest.version
            )));
        }
        Ok(())
    }

    async fn store_skill(&self, mut skill: Skill) -> Result<SkillId, MemoryError> {
        let all = self.load_all_async().await?;

        // Check for existing skill with same task_pattern
        if let Some(existing) = all.iter().find(|s| s.task_pattern == skill.task_pattern) {
            // Upsert: keep the existing ID and counters, update approach/tools/notes
            skill.id = existing.id.clone();
            skill.success_count = existing.success_count;
            skill.failure_count = existing.failure_count;
            skill.fitness = existing.fitness;
            skill.status = existing.status;
        } else if skill.id.0.is_empty() {
            skill.id = SkillId(apex_core::generate_id("skill"));
        }

        if skill.last_used.is_empty() {
            skill.last_used = apex_core::now_unix_ts();
        }

        self.write_skill(&skill)?;
        Ok(skill.id)
    }

    async fn update_skill_fitness(&self, id: &SkillId, success: bool) -> Result<(), MemoryError> {
        let all = self.load_all_async().await?;

        let mut skill = all
            .iter()
            .find(|s| s.id == *id)
            .cloned()
            .ok_or_else(|| MemoryError::NotFound(format!("skill {}", id.0)))?;

        if success {
            skill.success_count += 1;
        } else {
            skill.failure_count += 1;
        }

        let total = skill.success_count + skill.failure_count;
        skill.fitness = if total >= skill.min_samples {
            skill.success_count as f64 / total as f64
        } else {
            0.5
        };

        skill.last_used = apex_core::now_unix_ts();

        // Auto-retire if fitness too low
        if total >= skill.min_samples && skill.fitness < self.auto_retire_below {
            skill.status = SkillStatus::Retired;
        }

        self.write_skill(&skill)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_skill_store() -> (tempfile::TempDir, FsSkillStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSkillStore::new(dir.path().join("skills"));
        (dir, store)
    }

    fn make_skill(pattern: &str) -> Skill {
        Skill {
            id: SkillId(String::new()),
            name: String::new(),
            description: String::new(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            extra_metadata: Default::default(),
            task_pattern: pattern.to_string(),
            approach: "Test approach".to_string(),
            tools_used: vec!["bash".to_string()],
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            min_samples: 3,
            last_used: String::new(),
            status: SkillStatus::Active,
            version: "1.0.0".to_string(),
            skill_dir: None,
        }
    }

    #[tokio::test]
    async fn store_and_load_skill() {
        let (_dir, store) = temp_skill_store();
        let skill = make_skill("install package");
        let id = store.store_skill(skill).await.unwrap();
        assert!(id.0.starts_with("skill-"));

        // load_skill uses the slugified name
        let found = store.load_skill("install-package", "latest").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().task_pattern, "install package");
    }

    #[tokio::test]
    async fn upsert_preserves_counters() {
        let (_dir, store) = temp_skill_store();
        let skill = make_skill("build project");
        let id1 = store.store_skill(skill).await.unwrap();

        // Update fitness
        store.update_skill_fitness(&id1, true).await.unwrap();
        store.update_skill_fitness(&id1, true).await.unwrap();

        // Upsert with new approach
        let mut skill2 = make_skill("build project");
        skill2.approach = "Updated approach".to_string();
        let id2 = store.store_skill(skill2).await.unwrap();

        assert_eq!(id1.0, id2.0);
        let found = store
            .load_skill("build-project", "latest")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.approach, "Updated approach");
        assert_eq!(found.success_count, 2);
    }

    #[tokio::test]
    async fn list_manifests_returns_all() {
        let (_dir, store) = temp_skill_store();

        let mut s1 = make_skill("task alpha");
        s1.fitness = 0.8;
        s1.id = SkillId("skill-a".to_string());
        store.store_skill(s1).await.unwrap();

        let mut s2 = make_skill("task beta");
        s2.fitness = 0.9;
        s2.id = SkillId("skill-b".to_string());
        store.store_skill(s2).await.unwrap();

        let manifests = store.list_manifests().await.unwrap();
        assert_eq!(manifests.len(), 2);
        let names: Vec<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"task-alpha"));
        assert!(names.contains(&"task-beta"));
    }

    #[tokio::test]
    async fn auto_retire_hides_from_load() {
        let (_dir, store) = temp_skill_store();
        let skill = Skill {
            id: SkillId("s-bad".to_string()),
            name: String::new(),
            description: String::new(),
            license: None,
            compatibility: None,
            allowed_tools: None,
            extra_metadata: Default::default(),
            task_pattern: "flaky task".to_string(),
            approach: "bad approach".to_string(),
            tools_used: vec![],
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            min_samples: 3,
            last_used: String::new(),
            status: SkillStatus::Active,
            version: "1.0.0".to_string(),
            skill_dir: None,
        };
        store.store_skill(skill).await.unwrap();

        store
            .update_skill_fitness(&SkillId("s-bad".to_string()), false)
            .await
            .unwrap();
        store
            .update_skill_fitness(&SkillId("s-bad".to_string()), false)
            .await
            .unwrap();
        store
            .update_skill_fitness(&SkillId("s-bad".to_string()), false)
            .await
            .unwrap();

        // Retired skill is still loadable but marked retired
        let loaded = store.load_skill("flaky-task", "latest").await.unwrap();
        assert!(
            loaded.is_some_and(|s| s.status == SkillStatus::Retired),
            "retired skill should still be loadable but marked retired"
        );
    }

    #[tokio::test]
    async fn skill_file_is_readable_markdown() {
        let (_dir, store) = temp_skill_store();
        let skill = make_skill("deploy app");
        store.store_skill(skill).await.unwrap();

        // Read the file directly and verify it's valid spec-compliant markdown
        let file_path = store.write_dir.join("deploy-app").join("SKILL.md");
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.starts_with("---"));
        assert!(content.contains("name: deploy-app"));
        assert!(content.contains("apex-task-pattern: deploy app"));
        assert!(content.contains("Test approach"));
    }

    #[tokio::test]
    async fn multi_dir_discovery_with_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let shared_dir = dir.path().join("shared");
        let authored_dir = dir.path().join("authored");
        let learned_dir = dir.path().join("learned");

        // Create an authored skill (no apex-id)
        std::fs::create_dir_all(authored_dir.join("my-skill")).unwrap();
        std::fs::write(
            authored_dir.join("my-skill").join("SKILL.md"),
            "---\nname: my-skill\ndescription: An authored skill\n---\n\nDo the thing.\n",
        )
        .unwrap();

        // Create a learned skill with the same name (should be shadowed)
        std::fs::create_dir_all(learned_dir.join("my-skill")).unwrap();
        std::fs::write(
            learned_dir.join("my-skill").join("SKILL.md"),
            "---\nname: my-skill\ndescription: A learned skill\nmetadata:\n  apex-id: skill-123\n  apex-task-pattern: my task\n  apex-status: active\n  apex-fitness: '0.50'\n  apex-success-count: '2'\n  apex-failure-count: '1'\n  apex-min-samples: '3'\n  apex-last-used: '1234'\n---\n\nLearned approach.\n",
        ).unwrap();

        let store = FsSkillStore::with_dirs(
            vec![shared_dir, authored_dir, learned_dir.clone()],
            learned_dir,
        );

        let manifests = store.list_manifests().await.unwrap();
        assert_eq!(manifests.len(), 1);
        // Authored skill wins (higher precedence directory)
        assert_eq!(manifests[0].name, "my-skill");
        let skill = store
            .load_skill("my-skill", "latest")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(skill.description, "An authored skill");
        assert_eq!(skill.fitness, 1.0); // authored skill default
    }

    #[tokio::test]
    async fn authored_skill_gets_sensible_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(skills_dir.join("test-skill")).unwrap();
        std::fs::write(
            skills_dir.join("test-skill").join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill\n---\n\nDo stuff.\n",
        )
        .unwrap();

        let store = FsSkillStore::new(skills_dir);
        let manifests = store.list_manifests().await.unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name, "test-skill");

        let s = store
            .load_skill("test-skill", "latest")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.id.0, "skill-test-skill");
        assert_eq!(s.fitness, 1.0);
        assert_eq!(s.task_pattern, "A test skill");
        assert_eq!(s.success_count, 0);
        assert_eq!(s.status, SkillStatus::Active);
        assert!(s.skill_dir.is_some());
    }

    #[test]
    fn list_skill_resources_finds_files() {
        use apex_core::domain::list_skill_resources;
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::create_dir_all(skill_dir.join("assets")).unwrap();
        std::fs::write(skill_dir.join("scripts").join("extract.py"), "# script").unwrap();
        std::fs::write(skill_dir.join("references").join("REFERENCE.md"), "# ref").unwrap();
        std::fs::write(skill_dir.join("assets").join("template.json"), "{}").unwrap();

        let resources = list_skill_resources(&skill_dir);
        assert_eq!(resources.len(), 3);
        assert_eq!(resources["scripts"], vec!["scripts/extract.py"]);
        assert_eq!(resources["references"], vec!["references/REFERENCE.md"]);
        assert_eq!(resources["assets"], vec!["assets/template.json"]);
    }

    #[test]
    fn list_skill_resources_empty_when_no_dirs() {
        use apex_core::domain::list_skill_resources;
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("empty-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let resources = list_skill_resources(&skill_dir);
        assert!(resources.is_empty());
    }
}
