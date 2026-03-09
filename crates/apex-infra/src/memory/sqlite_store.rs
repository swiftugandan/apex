use std::path::Path;

use async_trait::async_trait;
use rusqlite::Connection;
use tokio::sync::Mutex;

use apex_core::domain::{CalibrationData, Fact, FactId};
use apex_core::error::MemoryError;
use apex_core::ports::MemoryStore;

pub struct SqliteMemoryStore {
    conn: Mutex<Connection>,
    confidence_half_life_days: f64,
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
