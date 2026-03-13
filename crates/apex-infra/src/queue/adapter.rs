use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use apex_core::domain::{
    ClaimedTask, MessageHeaders, MessageType, QueueDepth, QueueMessage, QueueMessageMeta,
    ReapResult,
};
use apex_core::error::QueueError;
use apex_core::ports::Queue;

use rfbmq_core::{ClaimedMessage, Header, Message, MessageId, Queue as RfbmqQueue};

pub struct RfbmqAdapter {
    queue: RfbmqQueue,
}

impl RfbmqAdapter {
    /// Initialize a new queue at the given root directory.
    pub fn init(root: &Path) -> Result<Self, QueueError> {
        let queue =
            RfbmqQueue::init(root, false, 10_000).map_err(|e| QueueError::Io(e.to_string()))?;
        Ok(Self { queue })
    }

    /// Open an existing queue.
    pub fn open(root: &Path) -> Result<Self, QueueError> {
        let queue = RfbmqQueue::open(root).map_err(|e| QueueError::NotFound(e.to_string()))?;
        Ok(Self { queue })
    }

    /// Initialize or open an existing queue.
    pub fn init_or_open(root: &Path) -> Result<Self, QueueError> {
        Self::init(root).or_else(|_| Self::open(root))
    }

    /// Move all failed messages back to pending with retry counts reset.
    pub fn retry_all_failed(&self) -> Result<u32, QueueError> {
        self.queue
            .retry_all_failed()
            .map_err(|e| QueueError::Io(e.to_string()))
    }
}

#[async_trait]
impl Queue for RfbmqAdapter {
    async fn push(&self, msg: QueueMessage) -> Result<String, QueueError> {
        let depends_on: Vec<MessageId> = msg
            .headers
            .depends_on
            .iter()
            .filter_map(|s| s.parse::<MessageId>().ok())
            .collect();

        let mut rfbmq_msg = Message {
            header: Header {
                correlation_id: Some(msg.headers.correlation_id.clone()),
                depends_on,
                custom: headers_to_custom(&msg.headers),
                ..Header::default()
            },
            body: msg.body,
        };

        let id = self
            .queue
            .enqueue(&mut rfbmq_msg)
            .map_err(|e| QueueError::Io(e.to_string()))?;

        Ok(id.to_string())
    }

    async fn pop(&self) -> Result<Option<ClaimedTask>, QueueError> {
        // Use dependency-aware list_ready to find messages whose deps are satisfied
        let ready_ids = self
            .queue
            .list_ready()
            .map_err(|e| QueueError::Io(e.to_string()))?;

        if ready_ids.is_empty() {
            return Ok(None);
        }

        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Try to claim the first available ready message
        for ready_id in &ready_ids {
            let claimed = self
                .queue
                .dequeue_id(ready_id)
                .map_err(|e| QueueError::Io(e.to_string()))?;

            let claimed = match claimed {
                Some(c) => c,
                None => continue, // Another worker claimed it
            };

            let rfbmq_msg =
                Message::from_file(claimed.path()).map_err(|e| QueueError::Parse(e.to_string()))?;

            // Check Not-Before: if the message has a future delivery time, put it back
            if let Some(not_before) = parse_not_before(&rfbmq_msg.header.custom) {
                if now_epoch < not_before {
                    // Not ready yet — put it back
                    self.queue
                        .fail(&claimed)
                        .map_err(|e| QueueError::Io(e.to_string()))?;
                    continue;
                }
            }

            let headers = custom_to_headers(
                &rfbmq_msg.header.custom,
                &rfbmq_msg.header.correlation_id,
                rfbmq_msg.header.retry_count,
            );

            return Ok(Some(ClaimedTask {
                id: claimed.id().to_string(),
                claim_path: claimed.path().to_string_lossy().into_owned(),
                headers,
                body: rfbmq_msg.body,
            }));
        }

        Ok(None)
    }

    async fn update_body(&self, claimed: &ClaimedTask, new_body: &str) -> Result<(), QueueError> {
        let claim_path = Path::new(&claimed.claim_path);
        let mut rfbmq_msg =
            Message::from_file(claim_path).map_err(|e| QueueError::Parse(e.to_string()))?;

        rfbmq_msg.body = new_body.to_string();

        let buf = rfbmq_msg
            .serialize()
            .map_err(|e| QueueError::Io(e.to_string()))?;

        let tmp_dir = self.queue.root().join(".tmp");
        let tmp_name = format!("update-{}.md", claimed.id);

        rfbmq_core::fs_utils::durable_write_rename(
            &tmp_dir,
            &tmp_name,
            claim_path,
            buf.as_bytes(),
            self.queue.file_mode(),
            self.queue.fsync_mode(),
        )
        .map_err(|e| QueueError::Io(e.to_string()))?;

        Ok(())
    }

    async fn ack(&self, claimed: &ClaimedTask) -> Result<(), QueueError> {
        let claim_path = Path::new(&claimed.claim_path);
        let rfbmq_claimed =
            ClaimedMessage::from_path(claim_path).map_err(|e| QueueError::Io(e.to_string()))?;

        self.queue
            .complete(&rfbmq_claimed)
            .map_err(|e| QueueError::Io(e.to_string()))?;

        Ok(())
    }

    async fn nack(&self, claimed: &ClaimedTask) -> Result<(), QueueError> {
        let claim_path = Path::new(&claimed.claim_path);
        let rfbmq_claimed =
            ClaimedMessage::from_path(claim_path).map_err(|e| QueueError::Io(e.to_string()))?;

        self.queue
            .fail(&rfbmq_claimed)
            .map_err(|e| QueueError::Io(e.to_string()))?;

        Ok(())
    }

    async fn nack_with_delay(
        &self,
        claimed: &ClaimedTask,
        delay: Duration,
    ) -> Result<(), QueueError> {
        let claim_path = Path::new(&claimed.claim_path);

        // Stamp a Not-Before header into the message before requeueing
        let not_before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + delay.as_secs();

        let mut rfbmq_msg =
            Message::from_file(claim_path).map_err(|e| QueueError::Parse(e.to_string()))?;

        // Remove any existing Not-Before, then add the new one
        rfbmq_msg
            .header
            .custom
            .retain(|l| !l.starts_with("Not-Before:"));
        rfbmq_msg
            .header
            .custom
            .push(format!("Not-Before: {not_before}"));

        // Write updated message back before requeueing
        let buf = rfbmq_msg
            .serialize()
            .map_err(|e| QueueError::Io(e.to_string()))?;
        let tmp_dir = self.queue.root().join(".tmp");
        let tmp_name = format!("delay-{}.md", claimed.id);
        rfbmq_core::fs_utils::durable_write_rename(
            &tmp_dir,
            &tmp_name,
            claim_path,
            buf.as_bytes(),
            self.queue.file_mode(),
            self.queue.fsync_mode(),
        )
        .map_err(|e| QueueError::Io(e.to_string()))?;

        // Now nack (requeue) it
        let rfbmq_claimed =
            ClaimedMessage::from_path(claim_path).map_err(|e| QueueError::Io(e.to_string()))?;
        self.queue
            .fail(&rfbmq_claimed)
            .map_err(|e| QueueError::Io(e.to_string()))?;

        Ok(())
    }

    async fn reject(&self, claimed: &ClaimedTask) -> Result<(), QueueError> {
        let claim_path = std::path::Path::new(&claimed.claim_path);
        let failed_path = self
            .queue
            .root()
            .join("failed")
            .join(format!("{}.md", claimed.id));

        rfbmq_core::fs_utils::durable_rename(claim_path, &failed_path, self.queue.fsync_mode())
            .map_err(|e| QueueError::Io(e.to_string()))?;

        Ok(())
    }

    async fn depth(&self) -> Result<QueueDepth, QueueError> {
        let total = self
            .queue
            .depth()
            .map_err(|e| QueueError::Io(e.to_string()))?;

        Ok(QueueDepth {
            pending: total as u32,
            processing: 0,
        })
    }

    async fn reap(&self) -> Result<ReapResult, QueueError> {
        let reaped = self
            .queue
            .reap()
            .map_err(|e| QueueError::Io(e.to_string()))?;

        Ok(ReapResult {
            lease_reaped: reaped,
        })
    }

    async fn list_done(&self, correlation_id: &str) -> Result<Vec<String>, QueueError> {
        let done_dir = self.queue.root().join("done");
        if !done_dir.exists() {
            return Ok(Vec::new());
        }

        let mut result = Vec::new();
        let entries = std::fs::read_dir(&done_dir).map_err(|e| QueueError::Io(e.to_string()))?;

        for entry in entries {
            let entry = entry.map_err(|e| QueueError::Io(e.to_string()))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            let msg = Message::from_file(&path).map_err(|e| QueueError::Parse(e.to_string()))?;
            if msg.header.correlation_id.as_deref() == Some(correlation_id) {
                if let Some(id) = msg.header.id {
                    result.push(id.to_string());
                }
            }
        }

        Ok(result)
    }

    async fn read_done_body(&self, id: &str) -> Result<String, QueueError> {
        let done_path = self.queue.root().join("done").join(format!("{id}.md"));
        let msg =
            Message::from_file(&done_path).map_err(|e| QueueError::NotFound(e.to_string()))?;
        Ok(msg.body)
    }

    async fn list_with_state(&self, state: &str) -> Result<Vec<QueueMessageMeta>, QueueError> {
        let dir = self.queue.root().join(state);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let entries = std::fs::read_dir(&dir).map_err(|e| QueueError::Io(e.to_string()))?;
        let mut result = Vec::new();

        for entry in entries {
            let entry = entry.map_err(|e| QueueError::Io(e.to_string()))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            if let Some(meta) = message_meta_from_path(&path) {
                result.push(meta);
            }
        }

        Ok(result)
    }
}

fn message_meta_from_path(path: &Path) -> Option<QueueMessageMeta> {
    let msg = Message::from_file(path).ok()?;
    let id = msg
        .header
        .id
        .as_ref()
        .map(|i| i.to_string())
        .unwrap_or_else(|| "???".to_string());
    let correlation_id = msg
        .header
        .correlation_id
        .clone()
        .unwrap_or_else(|| "-".to_string());
    let type_label = msg
        .header
        .custom
        .iter()
        .find(|l| l.starts_with("Type:"))
        .map(|l| l.trim_start_matches("Type:").trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let depends_on: Vec<String> = msg
        .header
        .depends_on
        .iter()
        .map(|d| d.to_string())
        .collect();

    Some(QueueMessageMeta {
        id,
        type_label,
        correlation_id,
        depends_on,
    })
}

fn parse_not_before(custom: &[String]) -> Option<u64> {
    custom
        .iter()
        .find(|l| l.starts_with("Not-Before:"))
        .and_then(|l| l.trim_start_matches("Not-Before:").trim().parse().ok())
}

fn headers_to_custom(headers: &MessageHeaders) -> Vec<String> {
    let type_str = match headers.message_type {
        MessageType::Task => "task",
        MessageType::Goal => "goal",
        MessageType::Subtask => "subtask",
        MessageType::Continuation => "continuation",
    };
    vec![
        format!("Type: {}", type_str),
        format!("Depth: {}", headers.depth),
    ]
}

fn custom_to_headers(
    custom: &[String],
    correlation_id: &Option<String>,
    retry_count: u32,
) -> MessageHeaders {
    let mut message_type = MessageType::Task;
    let mut depth: u32 = 0;

    for line in custom {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "Type" => {
                    message_type = match value {
                        "goal" => MessageType::Goal,
                        "subtask" => MessageType::Subtask,
                        "continuation" => MessageType::Continuation,
                        _ => MessageType::Task,
                    };
                }
                "Depth" => {
                    if let Ok(d) = value.parse() {
                        depth = d;
                    }
                }
                _ => {}
            }
        }
    }

    MessageHeaders {
        message_type,
        correlation_id: correlation_id.clone().unwrap_or_default(),
        depth,
        retry_count,
        depends_on: vec![],
        skills: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_msg(body: &str) -> QueueMessage {
        QueueMessage {
            headers: MessageHeaders {
                message_type: MessageType::Task,
                correlation_id: "corr-001".to_string(),
                depth: 0,
                retry_count: 0,
                depends_on: vec![],
                skills: vec![],
            },
            body: body.to_string(),
        }
    }

    #[test]
    fn init_creates_queue_structure() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("q");
        let _adapter = RfbmqAdapter::init(&root).unwrap();

        assert!(root.join("pending").exists());
        assert!(root.join("processing").exists());
        assert!(root.join("done").exists());
        assert!(root.join("failed").exists());
        assert!(root.join(".tmp").exists());
    }

    #[test]
    fn init_or_open_creates_then_reopens() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("q");

        let _a1 = RfbmqAdapter::init_or_open(&root).unwrap();
        drop(_a1);
        let _a2 = RfbmqAdapter::init_or_open(&root).unwrap();
    }

    #[tokio::test]
    async fn push_pop_ack_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("q");
        let adapter = RfbmqAdapter::init(&root).unwrap();

        let id = adapter.push(make_msg("hello world")).await.unwrap();
        assert!(!id.is_empty());

        let claimed = adapter.pop().await.unwrap().expect("expected a message");
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.body, "hello world");
        assert_eq!(claimed.headers.correlation_id, "corr-001");
        assert_eq!(claimed.headers.depth, 0);

        adapter.ack(&claimed).await.unwrap();

        let none = adapter.pop().await.unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn push_pop_nack_requeues() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("q");
        let adapter = RfbmqAdapter::init(&root).unwrap();

        adapter.push(make_msg("retry me")).await.unwrap();

        let claimed = adapter.pop().await.unwrap().unwrap();
        adapter.nack(&claimed).await.unwrap();

        // Message should be back in pending
        let reclaimed = adapter
            .pop()
            .await
            .unwrap()
            .expect("expected requeued message");
        assert_eq!(reclaimed.body, "retry me");
        adapter.ack(&reclaimed).await.unwrap();
    }

    #[tokio::test]
    async fn depth_returns_correct_counts() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("q");
        let adapter = RfbmqAdapter::init(&root).unwrap();

        let d = adapter.depth().await.unwrap();
        assert_eq!(d.pending, 0);

        adapter.push(make_msg("a")).await.unwrap();
        adapter.push(make_msg("b")).await.unwrap();

        let d = adapter.depth().await.unwrap();
        assert_eq!(d.pending, 2);

        let claimed = adapter.pop().await.unwrap().unwrap();
        let d = adapter.depth().await.unwrap();
        // depth() includes both pending and processing
        assert_eq!(d.pending, 2);

        adapter.ack(&claimed).await.unwrap();
        let d = adapter.depth().await.unwrap();
        assert_eq!(d.pending, 1);
    }

    #[tokio::test]
    async fn update_body_rewrites_message() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("q");
        let adapter = RfbmqAdapter::init(&root).unwrap();

        adapter.push(make_msg("original")).await.unwrap();
        let claimed = adapter.pop().await.unwrap().unwrap();
        assert_eq!(claimed.body, "original");

        adapter.update_body(&claimed, "updated").await.unwrap();

        // Re-read the file to verify the body changed
        let msg = Message::from_file(Path::new(&claimed.claim_path)).unwrap();
        assert_eq!(msg.body, "updated");
        // Headers should still have our custom fields
        assert!(msg.header.correlation_id.as_deref() == Some("corr-001"));

        adapter.ack(&claimed).await.unwrap();
    }

    #[tokio::test]
    async fn reap_on_empty_queue() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("q");
        let adapter = RfbmqAdapter::init(&root).unwrap();

        let result = adapter.reap().await.unwrap();
        assert_eq!(result.lease_reaped, 0);
    }

    #[test]
    fn headers_roundtrip() {
        let headers = MessageHeaders {
            message_type: MessageType::Task,
            correlation_id: "abc-123".to_string(),
            depth: 3,
            retry_count: 1,
            depends_on: vec![],
            skills: vec![],
        };

        let custom = headers_to_custom(&headers);
        // retry_count comes from the RFBMQ header, not custom headers
        let restored = custom_to_headers(&custom, &Some("abc-123".to_string()), 1);

        assert_eq!(restored.correlation_id, "abc-123");
        assert_eq!(restored.depth, 3);
        assert_eq!(restored.retry_count, 1);
        assert!(matches!(restored.message_type, MessageType::Task));
    }
}
