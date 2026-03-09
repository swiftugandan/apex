use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;

use apex_core::domain::{slugify, Skill, SkillId, SkillStatus};
use apex_core::error::MemoryError;
use apex_core::ports::SkillStore;

pub struct FsSkillStore {
    dir: PathBuf,
    cache: Mutex<Option<Vec<Skill>>>,
    auto_retire_below: f64,
}

impl FsSkillStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cache: Mutex::new(None),
            auto_retire_below: 0.2,
        }
    }

    fn ensure_dir(&self) -> Result<(), MemoryError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| MemoryError::Database(format!("failed to create skills dir: {e}")))?;
        Ok(())
    }

    fn invalidate_cache(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }

    fn load_all(&self) -> Result<Vec<Skill>, MemoryError> {
        // Check cache first
        if let Ok(cache) = self.cache.lock() {
            if let Some(ref cached) = *cache {
                return Ok(cached.clone());
            }
        }

        self.ensure_dir()?;

        let mut results = Vec::new();
        let entries = std::fs::read_dir(&self.dir)
            .map_err(|e| MemoryError::Database(format!("failed to read skills dir: {e}")))?;

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
                Ok(skill) => results.push(skill),
                Err(_) => continue,
            }
        }

        // Populate cache
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(results.clone());
        }

        Ok(results)
    }

    fn write_skill(&self, skill: &Skill) -> Result<PathBuf, MemoryError> {
        self.ensure_dir()?;
        let name = if skill.name.is_empty() {
            slugify(&skill.task_pattern)
        } else {
            skill.name.clone()
        };
        let skill_dir = self.dir.join(&name);
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
    async fn store_skill(&self, mut skill: Skill) -> Result<SkillId, MemoryError> {
        let all = self.load_all()?;

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

    async fn find_skill(&self, task_pattern: &str) -> Result<Option<Skill>, MemoryError> {
        let all = self.load_all()?;
        let pattern_lower = task_pattern.to_lowercase();

        let mut best: Option<&Skill> = None;
        for skill in &all {
            if skill.status == SkillStatus::Retired {
                continue;
            }
            // Auto-retire check
            let total = skill.success_count + skill.failure_count;
            if total >= skill.min_samples && skill.fitness < self.auto_retire_below {
                continue;
            }
            if !skill.task_pattern.to_lowercase().contains(&pattern_lower) {
                continue;
            }
            if best.is_none() || skill.fitness > best.unwrap().fitness {
                best = Some(skill);
            }
        }

        Ok(best.cloned())
    }

    async fn list_skills(&self, limit: usize) -> Result<Vec<Skill>, MemoryError> {
        let mut all = self.load_all()?;
        // Sort by fitness descending
        all.sort_by(|a, b| b.fitness.partial_cmp(&a.fitness).unwrap_or(std::cmp::Ordering::Equal));
        let skills: Vec<Skill> = all.into_iter().take(limit).collect();
        Ok(skills)
    }

    async fn update_skill_fitness(&self, id: &SkillId, success: bool) -> Result<(), MemoryError> {
        let all = self.load_all()?;

        let mut skill = all
            .into_iter()
            .find(|s| s.id == *id)
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
            task_pattern: pattern.to_string(),
            approach: "Test approach".to_string(),
            tools_used: vec!["bash".to_string()],
            criteria_template: None,
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            min_samples: 3,
            last_used: String::new(),
            notes: String::new(),
            status: SkillStatus::Active,
        }
    }

    #[tokio::test]
    async fn store_and_find_skill() {
        let (_dir, store) = temp_skill_store();
        let skill = make_skill("install package");
        let id = store.store_skill(skill).await.unwrap();
        assert!(id.0.starts_with("skill-"));

        let found = store.find_skill("install").await.unwrap();
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
        let found = store.find_skill("build").await.unwrap().unwrap();
        assert_eq!(found.approach, "Updated approach");
        assert_eq!(found.success_count, 2);
    }

    #[tokio::test]
    async fn list_skills_sorted() {
        let (_dir, store) = temp_skill_store();

        let mut s1 = make_skill("task alpha");
        s1.fitness = 0.8;
        s1.id = SkillId("skill-a".to_string());
        store.store_skill(s1).await.unwrap();

        let mut s2 = make_skill("task beta");
        s2.fitness = 0.9;
        s2.id = SkillId("skill-b".to_string());
        store.store_skill(s2).await.unwrap();

        let skills = store.list_skills(10).await.unwrap();
        assert_eq!(skills.len(), 2);
        assert!(skills[0].fitness >= skills[1].fitness);
    }

    #[tokio::test]
    async fn auto_retire_filters_bad_skills() {
        let (_dir, store) = temp_skill_store();
        let skill = Skill {
            id: SkillId("s-bad".to_string()),
            name: String::new(),
            description: String::new(),
            task_pattern: "flaky task".to_string(),
            approach: "bad approach".to_string(),
            tools_used: vec![],
            criteria_template: None,
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            min_samples: 3,
            last_used: String::new(),
            notes: String::new(),
            status: SkillStatus::Active,
        };
        store.store_skill(skill).await.unwrap();

        store.update_skill_fitness(&SkillId("s-bad".to_string()), false).await.unwrap();
        store.update_skill_fitness(&SkillId("s-bad".to_string()), false).await.unwrap();
        store.update_skill_fitness(&SkillId("s-bad".to_string()), false).await.unwrap();

        let found = store.find_skill("flaky").await.unwrap();
        assert!(found.is_none(), "retired skill should be filtered out");
    }

    #[tokio::test]
    async fn skill_file_is_readable_markdown() {
        let (_dir, store) = temp_skill_store();
        let mut skill = make_skill("deploy app");
        skill.notes = "Works on Linux.".to_string();
        skill.criteria_template = Some("- command: `which app`\n  expect: exit_code 0".to_string());
        store.store_skill(skill).await.unwrap();

        // Read the file directly and verify it's valid markdown
        let file_path = store.dir.join("deploy-app").join("SKILL.md");
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.starts_with("---"));
        assert!(content.contains("task_pattern: \"deploy app\""));
        assert!(content.contains("## Approach"));
        assert!(content.contains("## Acceptance Criteria"));
        assert!(content.contains("## Notes"));
    }
}
