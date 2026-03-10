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
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(
                content, tags, content='facts', content_rowid='rowid'
            );
            CREATE TRIGGER IF NOT EXISTS facts_ai AFTER INSERT ON facts BEGIN
                INSERT INTO facts_fts(rowid, content, tags) VALUES (new.rowid, new.content, new.tags);
            END;
            CREATE TRIGGER IF NOT EXISTS facts_ad AFTER DELETE ON facts BEGIN
                INSERT INTO facts_fts(facts_fts, rowid, content, tags) VALUES('delete', old.rowid, old.content, old.tags);
            END;
            CREATE TRIGGER IF NOT EXISTS facts_au AFTER UPDATE ON facts BEGIN
                INSERT INTO facts_fts(facts_fts, rowid, content, tags) VALUES('delete', old.rowid, old.content, old.tags);
                INSERT INTO facts_fts(rowid, content, tags) VALUES (new.rowid, new.content, new.tags);
            END;",
        )
        .map_err(|e| MemoryError::Database(format!("failed to create tables: {e}")))?;

        // Populate FTS index for pre-existing databases upgraded to FTS5.
        // Only rebuilds when facts exist but FTS index is empty.
        let needs_rebuild: bool = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM facts) > 0 AND (SELECT COUNT(*) FROM facts_fts) = 0",
            [],
            |row| row.get(0),
        ).unwrap_or(false);
        if needs_rebuild {
            let _ = conn.execute_batch("INSERT INTO facts_fts(facts_fts) VALUES('rebuild');");
        }

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

    /// Sanitize a query string for FTS5 MATCH by escaping special operators.
    /// Wraps each token in double quotes to treat them as literals.
    fn sanitize_fts_query(query: &str) -> String {
        let tokens: Vec<String> = query
            .split_whitespace()
            .filter(|t| !t.is_empty())
            .map(|t| {
                // Escape double quotes within the token, then wrap in quotes
                let escaped = t.replace('"', "\"\"");
                format!("\"{escaped}\"")
            })
            .collect();
        tokens.join(" ")
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
        let half_life = self.confidence_half_life_days;

        type FactRow = (String, String, String, f64, String, String, String);
        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<FactRow> {
            Ok((
                row.get(0)?, row.get(1)?, row.get(2)?,
                row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?,
            ))
        };

        let rows_result = if query.is_empty() {
            let mut stmt = conn
                .prepare(
                    "SELECT id, content, source_job, confidence, created_at, last_verified, tags
                     FROM facts
                     ORDER BY confidence DESC
                     LIMIT ?1",
                )
                .map_err(|e| MemoryError::Database(e.to_string()))?;

            let result = stmt.query_map(rusqlite::params![limit as i64], &map_row)
                .map_err(|e| MemoryError::Database(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| MemoryError::Database(e.to_string()));
            result
        } else {
            let fts_query = Self::sanitize_fts_query(query);
            let fts_result = conn
                .prepare(
                    "SELECT f.id, f.content, f.source_job, f.confidence, f.created_at, f.last_verified, f.tags
                     FROM facts f
                     JOIN facts_fts fts ON f.rowid = fts.rowid
                     WHERE facts_fts MATCH ?1
                     ORDER BY bm25(facts_fts), f.confidence DESC
                     LIMIT ?2",
                )
                .and_then(|mut stmt| {
                    stmt.query_map(rusqlite::params![fts_query, limit as i64], &map_row)?
                        .collect::<Result<Vec<_>, _>>()
                });

            match fts_result {
                Ok(rows) => Ok(rows),
                Err(e) => {
                    eprintln!("warning: FTS5 query failed ({e}), falling back to LIKE");
                    let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                    let pattern = format!("%{escaped}%");
                    let mut stmt = conn
                        .prepare(
                            "SELECT id, content, source_job, confidence, created_at, last_verified, tags
                             FROM facts
                             WHERE content LIKE ?1 ESCAPE '\\' OR tags LIKE ?1 ESCAPE '\\'
                             ORDER BY confidence DESC
                             LIMIT ?2",
                        )
                        .map_err(|e| MemoryError::Database(e.to_string()))?;

                    let result = stmt.query_map(rusqlite::params![pattern, limit as i64], &map_row)
                        .map_err(|e| MemoryError::Database(e.to_string()))?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| MemoryError::Database(e.to_string()));
                    result
                }
            }
        };

        let raw_rows = rows_result?;
        let mut facts = Vec::new();
        for (id, content, source_job, confidence, created_at, last_verified, tags_json) in raw_rows
        {
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
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

    #[tokio::test]
    async fn fts_basic_search() {
        let (_dir, store) = open_temp_store();
        let fact1 = Fact {
            id: FactId(String::new()),
            content: "Rust async runtime uses tokio".to_string(),
            source_job: "job-010".to_string(),
            confidence: 0.9,
            created_at: String::new(),
            last_verified: String::new(),
            tags: vec!["rust".to_string()],
        };
        let fact2 = Fact {
            id: FactId(String::new()),
            content: "Python uses asyncio for concurrency".to_string(),
            source_job: "job-011".to_string(),
            confidence: 0.8,
            created_at: String::new(),
            last_verified: String::new(),
            tags: vec!["python".to_string()],
        };
        store.store_fact(fact1).await.unwrap();
        store.store_fact(fact2).await.unwrap();

        let results = store.query_facts("tokio", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("tokio"));
    }

    #[tokio::test]
    async fn fts_empty_query_returns_all() {
        let (_dir, store) = open_temp_store();
        store
            .store_fact(Fact {
                id: FactId(String::new()),
                content: "fact one".to_string(),
                source_job: "j1".to_string(),
                confidence: 0.9,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            })
            .await
            .unwrap();
        store
            .store_fact(Fact {
                id: FactId(String::new()),
                content: "fact two".to_string(),
                source_job: "j2".to_string(),
                confidence: 0.8,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            })
            .await
            .unwrap();

        let results = store.query_facts("", 10).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn fts_special_characters_handled() {
        let (_dir, store) = open_temp_store();
        store
            .store_fact(Fact {
                id: FactId(String::new()),
                content: "config uses key=value pairs".to_string(),
                source_job: "j1".to_string(),
                confidence: 1.0,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            })
            .await
            .unwrap();

        // Query with special chars should not crash
        let results = store.query_facts("key=value", 10).await.unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn fts_update_sync() {
        let (_dir, store) = open_temp_store();
        // Store a fact
        let id = store
            .store_fact(Fact {
                id: FactId("f-update".to_string()),
                content: "original content about dogs".to_string(),
                source_job: "j1".to_string(),
                confidence: 1.0,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            })
            .await
            .unwrap();

        // Update it (INSERT OR REPLACE)
        store
            .store_fact(Fact {
                id: id.clone(),
                content: "updated content about cats".to_string(),
                source_job: "j1".to_string(),
                confidence: 1.0,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            })
            .await
            .unwrap();

        // Old content should not be findable via FTS
        let results = store.query_facts("dogs", 10).await.unwrap();
        assert!(results.is_empty(), "old content should not be in FTS index");

        // New content should be findable
        let results = store.query_facts("cats", 10).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn sanitize_fts_query_wraps_tokens() {
        let result = SqliteMemoryStore::sanitize_fts_query("hello world");
        assert_eq!(result, "\"hello\" \"world\"");
    }

    #[test]
    fn sanitize_fts_query_escapes_quotes() {
        let result = SqliteMemoryStore::sanitize_fts_query("say \"hello\"");
        assert_eq!(result, "\"say\" \"\"\"hello\"\"\"");
    }

    #[tokio::test]
    async fn query_with_percent_literal() {
        let (_dir, store) = open_temp_store();
        // Store a fact containing a literal percent
        store
            .store_fact(Fact {
                id: FactId(String::new()),
                content: "100% complete".to_string(),
                source_job: "j1".to_string(),
                confidence: 1.0,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            })
            .await
            .unwrap();
        store
            .store_fact(Fact {
                id: FactId(String::new()),
                content: "unrelated fact".to_string(),
                source_job: "j2".to_string(),
                confidence: 1.0,
                created_at: String::new(),
                last_verified: String::new(),
                tags: vec![],
            })
            .await
            .unwrap();

        // Force LIKE fallback by using a query that will fail FTS5
        // The FTS5 query sanitizer wraps in quotes, so this should work through FTS5.
        // To test the LIKE fallback specifically, we'd need to break FTS5,
        // but we can at least test the normal path handles percent correctly.
        let results = store.query_facts("100%", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("100%"));
    }
}
