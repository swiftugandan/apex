use std::path::PathBuf;

use async_trait::async_trait;
use apex_core::domain::Scratchpad;
use apex_core::error::MemoryError;
use apex_core::ports::WorkingMemory;

pub struct FsScratchpadStore {
    base_dir: PathBuf,
}

impl FsScratchpadStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn path_for(&self, job_id: &str) -> PathBuf {
        self.base_dir.join(format!("{job_id}.md"))
    }
}

#[async_trait]
impl WorkingMemory for FsScratchpadStore {
    async fn load_or_create(&self, job_id: &str) -> Result<Scratchpad, MemoryError> {
        let path = self.path_for(job_id);
        if path.exists() {
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| MemoryError::Io(e.to_string()))?;
            Scratchpad::from_markdown(&content).map_err(MemoryError::Parse)
        } else {
            Ok(Scratchpad::new(job_id, ""))
        }
    }

    async fn save(&self, scratchpad: &Scratchpad) -> Result<(), MemoryError> {
        tokio::fs::create_dir_all(&self.base_dir)
            .await
            .map_err(|e| MemoryError::Io(e.to_string()))?;
        let path = self.path_for(&scratchpad.job_id);
        let md = scratchpad.to_markdown();
        tokio::fs::write(&path, md.as_bytes())
            .await
            .map_err(|e| MemoryError::Io(e.to_string()))
    }

    async fn exists(&self, job_id: &str) -> Result<bool, MemoryError> {
        Ok(self.path_for(job_id).exists())
    }

    async fn delete(&self, job_id: &str) -> Result<(), MemoryError> {
        let path = self.path_for(job_id);
        if path.exists() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| MemoryError::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn list_active(&self) -> Result<Vec<String>, MemoryError> {
        let mut ids = Vec::new();
        if !self.base_dir.exists() {
            return Ok(ids);
        }
        let mut entries = tokio::fs::read_dir(&self.base_dir)
            .await
            .map_err(|e| MemoryError::Io(e.to_string()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| MemoryError::Io(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    ids.push(stem.to_string());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_load_save_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsScratchpadStore::new(dir.path().to_path_buf());

        // load_or_create returns empty scratchpad for new job
        let pad = store.load_or_create("job-01").await.unwrap();
        assert_eq!(pad.job_id, "job-01");
        assert!(pad.goal.is_empty());

        // save and reload
        let mut pad = pad;
        pad.goal = "Build something".into();
        pad.notes.push("note 1".into());
        store.save(&pad).await.unwrap();

        let loaded = store.load_or_create("job-01").await.unwrap();
        assert_eq!(loaded.goal, "Build something");
        assert_eq!(loaded.notes, vec!["note 1"]);
    }

    #[tokio::test]
    async fn exists_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsScratchpadStore::new(dir.path().to_path_buf());

        assert!(!store.exists("job-02").await.unwrap());

        let pad = Scratchpad::new("job-02", "test");
        store.save(&pad).await.unwrap();
        assert!(store.exists("job-02").await.unwrap());

        store.delete("job-02").await.unwrap();
        assert!(!store.exists("job-02").await.unwrap());
    }

    #[tokio::test]
    async fn list_active_returns_sorted_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsScratchpadStore::new(dir.path().to_path_buf());

        store.save(&Scratchpad::new("job-c", "")).await.unwrap();
        store.save(&Scratchpad::new("job-a", "")).await.unwrap();
        store.save(&Scratchpad::new("job-b", "")).await.unwrap();

        let ids = store.list_active().await.unwrap();
        assert_eq!(ids, vec!["job-a", "job-b", "job-c"]);
    }

    #[tokio::test]
    async fn list_active_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsScratchpadStore::new(dir.path().join("nonexistent"));
        let ids = store.list_active().await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsScratchpadStore::new(dir.path().to_path_buf());
        store.delete("no-such-job").await.unwrap();
    }
}
