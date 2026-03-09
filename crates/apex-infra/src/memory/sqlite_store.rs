use std::path::Path;

use async_trait::async_trait;
use rusqlite::Connection;
use tokio::sync::Mutex;

use apex_core::domain::{CalibrationData, Fact, FactId, Skill, SkillId, Strategy, StrategyId};
use apex_core::error::MemoryError;
use apex_core::ports::MemoryStore;

pub struct SqliteMemoryStore {
    conn: Mutex<Connection>,
    confidence_half_life_days: f64,
    auto_retire_below: f64,
}

impl SqliteMemoryStore {
    pub fn open(db_path: &Path) -> Result<Self, MemoryError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MemoryError::Database(format!("failed to create dir: {e}")))?;
        }

        let conn = Connection::open(db_path)
            .map_err(|e| MemoryError::Database(format!("failed to open db: {e}")))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS facts (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                source_job TEXT NOT NULL DEFAULT '',
                confidence REAL NOT NULL DEFAULT 1.0,
                created_at TEXT NOT NULL,
                last_verified TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '[]'
            );
            CREATE TABLE IF NOT EXISTS skills (
                id TEXT PRIMARY KEY,
                task_pattern TEXT NOT NULL,
                approach TEXT NOT NULL DEFAULT '',
                tools_used TEXT NOT NULL DEFAULT '[]',
                criteria_template TEXT,
                success_count INTEGER NOT NULL DEFAULT 0,
                failure_count INTEGER NOT NULL DEFAULT 0,
                fitness REAL NOT NULL DEFAULT 0.0,
                min_samples INTEGER NOT NULL DEFAULT 3,
                last_used TEXT NOT NULL,
                notes TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS strategies (
                id TEXT PRIMARY KEY,
                goal_pattern TEXT NOT NULL,
                decomposition TEXT NOT NULL DEFAULT '',
                avg_subtasks REAL NOT NULL DEFAULT 0.0,
                avg_duration_secs REAL NOT NULL DEFAULT 0.0,
                success_count INTEGER NOT NULL DEFAULT 0,
                failure_count INTEGER NOT NULL DEFAULT 0,
                fitness REAL NOT NULL DEFAULT 0.0,
                notes TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS calibration (
                id TEXT PRIMARY KEY DEFAULT 'default',
                chars_per_token_prose REAL NOT NULL,
                chars_per_token_code REAL NOT NULL,
                chars_per_token_mixed REAL NOT NULL,
                sample_count INTEGER NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .map_err(|e| MemoryError::Database(format!("failed to create tables: {e}")))?;

        Ok(Self {
            conn: Mutex::new(conn),
            confidence_half_life_days: 30.0,
            auto_retire_below: 0.2,
        })
    }

    fn now_iso() -> String {
        apex_core::now_unix_ts()
    }

    fn generate_id(prefix: &str) -> String {
        apex_core::generate_id(prefix)
    }

    /// Decay confidence based on time since last verification.
    /// confidence * 2^(-(days_since_verified / half_life))
    fn decay_confidence(confidence: f64, last_verified: &str, half_life_days: f64) -> f64 {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let verified_secs: u64 = last_verified.parse().unwrap_or(now_secs);
        let days_elapsed = (now_secs.saturating_sub(verified_secs)) as f64 / 86400.0;
        confidence * (2.0_f64).powf(-(days_elapsed / half_life_days))
    }
}

#[async_trait]
impl MemoryStore for SqliteMemoryStore {
    async fn store_fact(&self, fact: Fact) -> Result<FactId, MemoryError> {
        let conn = self.conn.lock().await;
        let id = if fact.id.0.is_empty() {
            Self::generate_id("fact")
        } else {
            fact.id.0.clone()
        };
        let now = Self::now_iso();
        let tags_json =
            serde_json::to_string(&fact.tags).unwrap_or_else(|_| "[]".to_string());
        let created = if fact.created_at.is_empty() {
            &now
        } else {
            &fact.created_at
        };
        let verified = if fact.last_verified.is_empty() {
            &now
        } else {
            &fact.last_verified
        };
        conn.execute(
            "INSERT OR REPLACE INTO facts (id, content, source_job, confidence, created_at, last_verified, tags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![id, fact.content, fact.source_job, fact.confidence, created, verified, tags_json],
        )
        .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(FactId(id))
    }

    async fn query_facts(&self, query: &str, limit: usize) -> Result<Vec<Fact>, MemoryError> {
        let conn = self.conn.lock().await;
        let pattern = format!("%{query}%");
        let half_life = self.confidence_half_life_days;
        let mut stmt = conn
            .prepare(
                "SELECT id, content, source_job, confidence, created_at, last_verified, tags
                 FROM facts
                 WHERE content LIKE ?1 OR tags LIKE ?1
                 ORDER BY confidence DESC
                 LIMIT ?2",
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![pattern, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        let mut facts = Vec::new();
        for row in rows {
            let (id, content, source_job, confidence, created_at, last_verified, tags_json) =
                row.map_err(|e| MemoryError::Database(e.to_string()))?;
            let tags: Vec<String> =
                serde_json::from_str(&tags_json).unwrap_or_default();
            let decayed = Self::decay_confidence(confidence, &last_verified, half_life);
            facts.push(Fact {
                id: FactId(id),
                content,
                source_job,
                confidence: decayed,
                created_at,
                last_verified,
                tags,
            });
        }
        Ok(facts)
    }

    async fn verify_fact(&self, id: &FactId) -> Result<(), MemoryError> {
        let conn = self.conn.lock().await;
        let now = Self::now_iso();
        let updated = conn
            .execute(
                "UPDATE facts SET last_verified = ?1, confidence = 1.0 WHERE id = ?2",
                rusqlite::params![now, id.0],
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        if updated == 0 {
            return Err(MemoryError::NotFound(format!("fact {}", id.0)));
        }
        Ok(())
    }

    async fn store_skill(&self, skill: Skill) -> Result<SkillId, MemoryError> {
        let conn = self.conn.lock().await;
        let now = Self::now_iso();
        let tools_json =
            serde_json::to_string(&skill.tools_used).unwrap_or_else(|_| "[]".to_string());

        // Upsert: check if same task_pattern exists
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM skills WHERE task_pattern = ?1",
                rusqlite::params![skill.task_pattern],
                |row| row.get(0),
            )
            .ok();

        let id = if let Some(eid) = existing_id {
            conn.execute(
                "UPDATE skills SET approach = ?1, tools_used = ?2, criteria_template = ?3,
                 last_used = ?4, notes = ?5 WHERE id = ?6",
                rusqlite::params![
                    skill.approach,
                    tools_json,
                    skill.criteria_template,
                    now,
                    skill.notes,
                    eid
                ],
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;
            eid
        } else {
            let id = if skill.id.0.is_empty() {
                Self::generate_id("skill")
            } else {
                skill.id.0.clone()
            };
            conn.execute(
                "INSERT INTO skills (id, task_pattern, approach, tools_used, criteria_template,
                 success_count, failure_count, fitness, min_samples, last_used, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    id,
                    skill.task_pattern,
                    skill.approach,
                    tools_json,
                    skill.criteria_template,
                    skill.success_count,
                    skill.failure_count,
                    skill.fitness,
                    skill.min_samples,
                    now,
                    skill.notes
                ],
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;
            id
        };

        Ok(SkillId(id))
    }

    async fn find_skill(&self, task_pattern: &str) -> Result<Option<Skill>, MemoryError> {
        let conn = self.conn.lock().await;
        let pattern = format!("%{task_pattern}%");
        let retire_below = self.auto_retire_below;

        let mut stmt = conn
            .prepare(
                "SELECT id, task_pattern, approach, tools_used, criteria_template,
                 success_count, failure_count, fitness, min_samples, last_used, notes
                 FROM skills
                 WHERE task_pattern LIKE ?1
                 ORDER BY fitness DESC
                 LIMIT 1",
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        let result = stmt
            .query_row(rusqlite::params![pattern], |row| {
                Ok(Skill {
                    id: SkillId(row.get(0)?),
                    task_pattern: row.get(1)?,
                    approach: row.get(2)?,
                    tools_used: serde_json::from_str(&row.get::<_, String>(3)?)
                        .unwrap_or_default(),
                    criteria_template: row.get(4)?,
                    success_count: row.get(5)?,
                    failure_count: row.get(6)?,
                    fitness: row.get(7)?,
                    min_samples: row.get(8)?,
                    last_used: row.get(9)?,
                    notes: row.get(10)?,
                })
            })
            .ok();

        // Filter out retired skills
        if let Some(ref skill) = result {
            let total = skill.success_count + skill.failure_count;
            if total >= skill.min_samples && skill.fitness < retire_below {
                return Ok(None);
            }
        }

        Ok(result)
    }

    async fn list_skills(&self, limit: usize) -> Result<Vec<Skill>, MemoryError> {
        let conn = self.conn.lock().await;
        let limit = limit.min(1000) as i64;
        let mut stmt = conn
            .prepare(
                "SELECT id, task_pattern, approach, tools_used, criteria_template,
                 success_count, failure_count, fitness, min_samples, last_used, notes
                 FROM skills ORDER BY fitness DESC LIMIT ?1",
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![limit], |row| {
                Ok(Skill {
                    id: SkillId(row.get(0)?),
                    task_pattern: row.get(1)?,
                    approach: row.get(2)?,
                    tools_used: serde_json::from_str(&row.get::<_, String>(3)?)
                        .unwrap_or_default(),
                    criteria_template: row.get(4)?,
                    success_count: row.get(5)?,
                    failure_count: row.get(6)?,
                    fitness: row.get(7)?,
                    min_samples: row.get(8)?,
                    last_used: row.get(9)?,
                    notes: row.get(10)?,
                })
            })
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        let skills: Vec<Skill> = rows.filter_map(|r| r.ok()).collect();
        Ok(skills)
    }

    async fn update_skill_fitness(
        &self,
        id: &SkillId,
        success: bool,
    ) -> Result<(), MemoryError> {
        let conn = self.conn.lock().await;
        let now = Self::now_iso();

        let (s_count, f_count, min_samples): (u32, u32, u32) = conn
            .query_row(
                "SELECT success_count, failure_count, min_samples FROM skills WHERE id = ?1",
                rusqlite::params![id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        let (new_s, new_f) = if success {
            (s_count + 1, f_count)
        } else {
            (s_count, f_count + 1)
        };

        let total = new_s + new_f;
        let fitness = if total >= min_samples {
            new_s as f64 / total as f64
        } else {
            // Not enough samples yet, keep neutral
            0.5
        };

        conn.execute(
            "UPDATE skills SET success_count = ?1, failure_count = ?2, fitness = ?3, last_used = ?4 WHERE id = ?5",
            rusqlite::params![new_s, new_f, fitness, now, id.0],
        )
        .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(())
    }

    async fn store_strategy(&self, strategy: Strategy) -> Result<StrategyId, MemoryError> {
        let conn = self.conn.lock().await;

        // Upsert: check if same goal_pattern exists
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM strategies WHERE goal_pattern = ?1",
                rusqlite::params![strategy.goal_pattern],
                |row| row.get(0),
            )
            .ok();

        let id = if let Some(eid) = existing_id {
            conn.execute(
                "UPDATE strategies SET decomposition = ?1, avg_subtasks = ?2,
                 avg_duration_secs = ?3, notes = ?4 WHERE id = ?5",
                rusqlite::params![
                    strategy.decomposition,
                    strategy.avg_subtasks,
                    strategy.avg_duration_secs,
                    strategy.notes,
                    eid
                ],
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;
            eid
        } else {
            let id = if strategy.id.0.is_empty() {
                Self::generate_id("strat")
            } else {
                strategy.id.0.clone()
            };
            conn.execute(
                "INSERT INTO strategies (id, goal_pattern, decomposition, avg_subtasks,
                 avg_duration_secs, success_count, failure_count, fitness, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    id,
                    strategy.goal_pattern,
                    strategy.decomposition,
                    strategy.avg_subtasks,
                    strategy.avg_duration_secs,
                    strategy.success_count,
                    strategy.failure_count,
                    strategy.fitness,
                    strategy.notes
                ],
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;
            id
        };

        Ok(StrategyId(id))
    }

    async fn find_strategy(&self, goal: &str) -> Result<Option<Strategy>, MemoryError> {
        let conn = self.conn.lock().await;
        let pattern = format!("%{goal}%");

        let result = conn
            .query_row(
                "SELECT id, goal_pattern, decomposition, avg_subtasks, avg_duration_secs,
                 success_count, failure_count, fitness, notes
                 FROM strategies
                 WHERE goal_pattern LIKE ?1
                 ORDER BY fitness DESC
                 LIMIT 1",
                rusqlite::params![pattern],
                |row| {
                    Ok(Strategy {
                        id: StrategyId(row.get(0)?),
                        goal_pattern: row.get(1)?,
                        decomposition: row.get(2)?,
                        avg_subtasks: row.get(3)?,
                        avg_duration_secs: row.get(4)?,
                        success_count: row.get(5)?,
                        failure_count: row.get(6)?,
                        fitness: row.get(7)?,
                        notes: row.get(8)?,
                    })
                },
            )
            .ok();

        Ok(result)
    }

    async fn list_strategies(&self, limit: usize) -> Result<Vec<Strategy>, MemoryError> {
        let conn = self.conn.lock().await;
        let limit = limit.min(1000) as i64;
        let mut stmt = conn
            .prepare(
                "SELECT id, goal_pattern, decomposition, avg_subtasks, avg_duration_secs,
                 success_count, failure_count, fitness, notes
                 FROM strategies ORDER BY fitness DESC LIMIT ?1",
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![limit], |row| {
                Ok(Strategy {
                    id: StrategyId(row.get(0)?),
                    goal_pattern: row.get(1)?,
                    decomposition: row.get(2)?,
                    avg_subtasks: row.get(3)?,
                    avg_duration_secs: row.get(4)?,
                    success_count: row.get(5)?,
                    failure_count: row.get(6)?,
                    fitness: row.get(7)?,
                    notes: row.get(8)?,
                })
            })
            .map_err(|e| MemoryError::Database(e.to_string()))?;
        let strategies: Vec<Strategy> = rows.filter_map(|r| r.ok()).collect();
        Ok(strategies)
    }

    async fn update_strategy_fitness(
        &self,
        id: &StrategyId,
        success: bool,
    ) -> Result<(), MemoryError> {
        let conn = self.conn.lock().await;

        let (s_count, f_count): (u32, u32) = conn
            .query_row(
                "SELECT success_count, failure_count FROM strategies WHERE id = ?1",
                rusqlite::params![id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| MemoryError::Database(e.to_string()))?;

        let (new_s, new_f) = if success {
            (s_count + 1, f_count)
        } else {
            (s_count, f_count + 1)
        };

        let total = new_s + new_f;
        let fitness = if total > 0 {
            new_s as f64 / total as f64
        } else {
            0.0
        };

        conn.execute(
            "UPDATE strategies SET success_count = ?1, failure_count = ?2, fitness = ?3 WHERE id = ?4",
            rusqlite::params![new_s, new_f, fitness, id.0],
        )
        .map_err(|e| MemoryError::Database(e.to_string()))?;

        Ok(())
    }

    async fn persist_calibration(&self, data: &CalibrationData) -> Result<(), MemoryError> {
        let conn = self.conn.lock().await;
        let now = Self::now_iso();
        conn.execute(
            "INSERT OR REPLACE INTO calibration (id, chars_per_token_prose, chars_per_token_code, chars_per_token_mixed, sample_count, updated_at)
             VALUES ('default', ?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                data.chars_per_token_prose as f64,
                data.chars_per_token_code as f64,
                data.chars_per_token_mixed as f64,
                data.sample_count,
                now
            ],
        )
        .map_err(|e| MemoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn load_calibration(&self) -> Result<CalibrationData, MemoryError> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT chars_per_token_prose, chars_per_token_code, chars_per_token_mixed, sample_count
             FROM calibration WHERE id = 'default'",
            [],
            |row| {
                Ok(CalibrationData {
                    chars_per_token_prose: row.get::<_, f64>(0)? as f32,
                    chars_per_token_code: row.get::<_, f64>(1)? as f32,
                    chars_per_token_mixed: row.get::<_, f64>(2)? as f32,
                    sample_count: row.get(3)?,
                })
            },
        );
        match result {
            Ok(data) => Ok(data),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(CalibrationData::default()),
            Err(e) => Err(MemoryError::Database(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp_store() -> (tempfile::TempDir, SqliteMemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let store = SqliteMemoryStore::open(&db_path).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn store_and_query_fact() {
        let (_dir, store) = open_temp_store();
        let fact = Fact {
            id: FactId(String::new()),
            content: "curl supports HTTP/2".to_string(),
            source_job: "job-001".to_string(),
            confidence: 0.9,
            created_at: String::new(),
            last_verified: String::new(),
            tags: vec!["curl".to_string(), "http".to_string()],
        };
        let id = store.store_fact(fact).await.unwrap();
        assert!(id.0.starts_with("fact-"));

        let results = store.query_facts("curl", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "curl supports HTTP/2");
        assert!(results[0].confidence <= 0.9); // May have decayed slightly
    }

    #[tokio::test]
    async fn query_facts_by_tag() {
        let (_dir, store) = open_temp_store();
        let fact = Fact {
            id: FactId(String::new()),
            content: "some fact".to_string(),
            source_job: "job-002".to_string(),
            confidence: 1.0,
            created_at: String::new(),
            last_verified: String::new(),
            tags: vec!["networking".to_string()],
        };
        store.store_fact(fact).await.unwrap();

        let results = store.query_facts("networking", 10).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn verify_fact_resets_confidence() {
        let (_dir, store) = open_temp_store();
        let fact = Fact {
            id: FactId("f1".to_string()),
            content: "test fact".to_string(),
            source_job: "job-003".to_string(),
            confidence: 0.5,
            created_at: String::new(),
            last_verified: String::new(),
            tags: vec![],
        };
        store.store_fact(fact).await.unwrap();
        store.verify_fact(&FactId("f1".to_string())).await.unwrap();

        let results = store.query_facts("test fact", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        // Confidence should be ~1.0 after verify (with tiny decay)
        assert!(results[0].confidence > 0.99);
    }

    #[tokio::test]
    async fn store_and_find_skill() {
        let (_dir, store) = open_temp_store();
        let skill = Skill {
            id: SkillId(String::new()),
            task_pattern: "install package".to_string(),
            approach: "Use apt-get or brew".to_string(),
            tools_used: vec!["bash".to_string()],
            criteria_template: Some("### Deterministic\n- command: `which PKG`\n  expect: exit_code 0".to_string()),
            success_count: 0,
            failure_count: 0,
            fitness: 0.0,
            min_samples: 3,
            last_used: String::new(),
            notes: String::new(),
        };
        let id = store.store_skill(skill).await.unwrap();
        assert!(id.0.starts_with("skill-"));

        let found = store.find_skill("install").await.unwrap();
        assert!(found.is_some());
        let s = found.unwrap();
        assert_eq!(s.approach, "Use apt-get or brew");
        assert_eq!(s.tools_used, vec!["bash"]);
    }

    #[tokio::test]
    async fn skill_upsert_updates_existing() {
        let (_dir, store) = open_temp_store();
        let skill1 = Skill {
            id: SkillId(String::new()),
            task_pattern: "install package".to_string(),
            approach: "v1 approach".to_string(),
            tools_used: vec![],
            criteria_template: None,
            success_count: 0,
            failure_count: 0,
            fitness: 0.0,
            min_samples: 3,
            last_used: String::new(),
            notes: String::new(),
        };
        let id1 = store.store_skill(skill1).await.unwrap();

        let skill2 = Skill {
            id: SkillId(String::new()),
            task_pattern: "install package".to_string(),
            approach: "v2 approach".to_string(),
            tools_used: vec!["bash".to_string()],
            criteria_template: None,
            success_count: 0,
            failure_count: 0,
            fitness: 0.0,
            min_samples: 3,
            last_used: String::new(),
            notes: String::new(),
        };
        let id2 = store.store_skill(skill2).await.unwrap();

        // Should have updated same record
        assert_eq!(id1.0, id2.0);

        let found = store.find_skill("install").await.unwrap().unwrap();
        assert_eq!(found.approach, "v2 approach");
    }

    #[tokio::test]
    async fn update_skill_fitness_calculation() {
        let (_dir, store) = open_temp_store();
        let skill = Skill {
            id: SkillId("s1".to_string()),
            task_pattern: "build project".to_string(),
            approach: "cargo build".to_string(),
            tools_used: vec!["bash".to_string()],
            criteria_template: None,
            success_count: 0,
            failure_count: 0,
            fitness: 0.5,
            min_samples: 3,
            last_used: String::new(),
            notes: String::new(),
        };
        store.store_skill(skill).await.unwrap();

        // 3 successes -> fitness = 3/3 = 1.0 (after min_samples met)
        store.update_skill_fitness(&SkillId("s1".to_string()), true).await.unwrap();
        store.update_skill_fitness(&SkillId("s1".to_string()), true).await.unwrap();
        store.update_skill_fitness(&SkillId("s1".to_string()), true).await.unwrap();

        let found = store.find_skill("build").await.unwrap().unwrap();
        assert!((found.fitness - 1.0).abs() < 0.01);
        assert_eq!(found.success_count, 3);
    }

    #[tokio::test]
    async fn auto_retire_filters_bad_skills() {
        let (_dir, store) = open_temp_store();
        let skill = Skill {
            id: SkillId("s-bad".to_string()),
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
        };
        store.store_skill(skill).await.unwrap();

        // 3 failures -> fitness = 0/3 = 0.0
        store.update_skill_fitness(&SkillId("s-bad".to_string()), false).await.unwrap();
        store.update_skill_fitness(&SkillId("s-bad".to_string()), false).await.unwrap();
        store.update_skill_fitness(&SkillId("s-bad".to_string()), false).await.unwrap();

        let found = store.find_skill("flaky").await.unwrap();
        assert!(found.is_none(), "retired skill should be filtered out");
    }

    #[tokio::test]
    async fn store_and_find_strategy() {
        let (_dir, store) = open_temp_store();
        let strategy = Strategy {
            id: StrategyId(String::new()),
            goal_pattern: "deploy application".to_string(),
            decomposition: "1. Build 2. Test 3. Deploy".to_string(),
            avg_subtasks: 3.0,
            avg_duration_secs: 120.0,
            success_count: 1,
            failure_count: 0,
            fitness: 1.0,
            notes: String::new(),
        };
        store.store_strategy(strategy).await.unwrap();

        let found = store.find_strategy("deploy").await.unwrap();
        assert!(found.is_some());
        let st = found.unwrap();
        assert_eq!(st.decomposition, "1. Build 2. Test 3. Deploy");
        assert!((st.avg_subtasks - 3.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn update_strategy_fitness_calculation() {
        let (_dir, store) = open_temp_store();
        let strategy = Strategy {
            id: StrategyId("st1".to_string()),
            goal_pattern: "setup env".to_string(),
            decomposition: "steps".to_string(),
            avg_subtasks: 2.0,
            avg_duration_secs: 60.0,
            success_count: 0,
            failure_count: 0,
            fitness: 0.0,
            notes: String::new(),
        };
        store.store_strategy(strategy).await.unwrap();

        store.update_strategy_fitness(&StrategyId("st1".to_string()), true).await.unwrap();
        store.update_strategy_fitness(&StrategyId("st1".to_string()), false).await.unwrap();

        let found = store.find_strategy("setup").await.unwrap().unwrap();
        assert!((found.fitness - 0.5).abs() < 0.01);
        assert_eq!(found.success_count, 1);
        assert_eq!(found.failure_count, 1);
    }

    #[test]
    fn confidence_decay_math() {
        // At exactly half_life days, confidence should halve
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let thirty_days_ago = (now_secs - 30 * 86400).to_string();

        let decayed = SqliteMemoryStore::decay_confidence(1.0, &thirty_days_ago, 30.0);
        assert!((decayed - 0.5).abs() < 0.05, "expected ~0.5, got {decayed}");
    }
}
