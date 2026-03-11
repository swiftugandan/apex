use std::sync::Arc;
use tokio::sync::Mutex;

use async_trait::async_trait;

use apex_core::domain::Scratchpad;
use apex_core::error::MemoryError;
use apex_core::ports::WorkingMemory;

/// Per-claim working memory adapter that keeps the in-memory `Scratchpad`
/// mutex in sync with a persistent backing store.
///
/// The agentic loop owns `Arc<Mutex<Scratchpad>>` for direct log-entry
/// writes.  `SessionWorkingMemory` wraps the *same* mutex so that
/// `MemoryToolRegistry`'s `working_memory_read` / `working_memory_update`
/// tools transparently read from and write to the shared in-memory state,
/// while also persisting every mutation to disk via the backing store.
pub struct SessionWorkingMemory {
    scratchpad: Arc<Mutex<Scratchpad>>,
    backing: Arc<dyn WorkingMemory>,
}

impl SessionWorkingMemory {
    pub fn new(scratchpad: Arc<Mutex<Scratchpad>>, backing: Arc<dyn WorkingMemory>) -> Self {
        Self {
            scratchpad,
            backing,
        }
    }
}

#[async_trait]
impl WorkingMemory for SessionWorkingMemory {
    /// Returns a clone of the in-memory scratchpad.
    ///
    /// The `job_id` parameter is accepted for trait compatibility but
    /// ignored — every call returns the session-scoped scratchpad.
    async fn load_or_create(&self, _job_id: &str) -> Result<Scratchpad, MemoryError> {
        Ok(self.scratchpad.lock().await.clone())
    }

    /// Replaces the in-memory scratchpad and persists to the backing store.
    async fn save(&self, scratchpad: &Scratchpad) -> Result<(), MemoryError> {
        *self.scratchpad.lock().await = scratchpad.clone();
        self.backing.save(scratchpad).await
    }

    async fn exists(&self, job_id: &str) -> Result<bool, MemoryError> {
        self.backing.exists(job_id).await
    }

    async fn delete(&self, job_id: &str) -> Result<(), MemoryError> {
        self.backing.delete(job_id).await
    }

    async fn list_active(&self) -> Result<Vec<String>, MemoryError> {
        self.backing.list_active().await
    }

    async fn reap_stale(&self, retention_days: u32) -> Result<Vec<String>, MemoryError> {
        self.backing.reap_stale(retention_days).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeBackingMemory {
        pads: Mutex<HashMap<String, Scratchpad>>,
    }

    impl FakeBackingMemory {
        fn new() -> Self {
            Self {
                pads: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl WorkingMemory for FakeBackingMemory {
        async fn load_or_create(&self, job_id: &str) -> Result<Scratchpad, MemoryError> {
            let pads = self.pads.lock().await;
            Ok(pads
                .get(job_id)
                .cloned()
                .unwrap_or_else(|| Scratchpad::new(job_id, "")))
        }
        async fn save(&self, scratchpad: &Scratchpad) -> Result<(), MemoryError> {
            self.pads
                .lock()
                .await
                .insert(scratchpad.job_id.clone(), scratchpad.clone());
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

    #[tokio::test]
    async fn load_returns_clone_from_mutex() {
        let pad = Scratchpad::new("job-1", "test goal");
        let scratchpad = Arc::new(Mutex::new(pad));
        let backing: Arc<dyn WorkingMemory> = Arc::new(FakeBackingMemory::new());
        let session = SessionWorkingMemory::new(Arc::clone(&scratchpad), backing);

        let loaded = session.load_or_create("ignored-id").await.unwrap();
        assert_eq!(loaded.goal, "test goal");
        assert_eq!(loaded.job_id, "job-1");
    }

    #[tokio::test]
    async fn save_updates_mutex_and_persists() {
        let pad = Scratchpad::new("job-2", "initial");
        let scratchpad = Arc::new(Mutex::new(pad));
        let backing = Arc::new(FakeBackingMemory::new());
        let session = SessionWorkingMemory::new(
            Arc::clone(&scratchpad),
            Arc::clone(&backing) as Arc<dyn WorkingMemory>,
        );

        let mut updated = session.load_or_create("any").await.unwrap();
        updated.goal = "updated goal".to_string();
        updated.notes.push("a note".to_string());
        session.save(&updated).await.unwrap();

        // Verify in-memory mutex was updated
        {
            let in_mem = scratchpad.lock().await;
            assert_eq!(in_mem.goal, "updated goal");
            assert_eq!(in_mem.notes, vec!["a note"]);
        }

        // Verify backing store received the write
        {
            let persisted = backing.pads.lock().await;
            let saved = persisted.get("job-2").expect("should be persisted");
            assert_eq!(saved.goal, "updated goal");
            assert_eq!(saved.notes, vec!["a note"]);
        }
    }

    #[tokio::test]
    async fn mutex_reflects_external_mutations() {
        let pad = Scratchpad::new("job-3", "original");
        let scratchpad = Arc::new(Mutex::new(pad));
        let backing: Arc<dyn WorkingMemory> = Arc::new(FakeBackingMemory::new());
        let session = SessionWorkingMemory::new(Arc::clone(&scratchpad), backing);

        // Simulate the agentic loop modifying the scratchpad directly
        {
            let mut guard = scratchpad.lock().await;
            guard.notes.push("log entry from agentic loop".to_string());
        }

        // Session should see the mutation
        let loaded = session.load_or_create("any").await.unwrap();
        assert_eq!(loaded.notes, vec!["log entry from agentic loop"]);
    }
}
