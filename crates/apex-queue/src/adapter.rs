use std::path::Path;

use async_trait::async_trait;

use apex_core::domain::{
    ClaimedTask, MessageHeaders, MessageType, QueueDepth, QueueMessage, ReapResult,
};
use apex_core::error::QueueError;
use apex_core::ports::Queue;

use rfbmq_core::{ClaimedMessage, Header, Message, Queue as RfbmqQueue};

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
}

#[async_trait]
impl Queue for RfbmqAdapter {
    async fn push(&self, msg: QueueMessage) -> Result<String, QueueError> {
        let mut rfbmq_msg = Message {
            header: Header {
                correlation_id: Some(msg.headers.correlation_id.clone()),
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
        let claimed = self
            .queue
            .dequeue()
            .map_err(|e| QueueError::Io(e.to_string()))?;

        let claimed = match claimed {
            Some(c) => c,
            None => return Ok(None),
        };

        let rfbmq_msg =
            Message::from_file(claimed.path()).map_err(|e| QueueError::Parse(e.to_string()))?;

        let headers = custom_to_headers(
            &rfbmq_msg.header.custom,
            &rfbmq_msg.header.correlation_id,
            rfbmq_msg.header.retry_count,
        );

        Ok(Some(ClaimedTask {
            id: claimed.id().to_string(),
            claim_path: claimed.path().to_string_lossy().into_owned(),
            headers,
            body: rfbmq_msg.body,
        }))
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
}

fn headers_to_custom(headers: &MessageHeaders) -> Vec<String> {
    let type_str = match headers.message_type {
        MessageType::Task => "task",
    };
    vec![
        format!("Type: {}", type_str),
        format!("Depth: {}", headers.depth),
        format!("Retry-Count: {}", headers.retry_count),
    ]
}

fn custom_to_headers(
    custom: &[String],
    correlation_id: &Option<String>,
    retry_count: u32,
) -> MessageHeaders {
    let mut message_type = MessageType::Task;
    let mut depth: u32 = 0;
    let mut custom_retry: Option<u32> = None;

    for line in custom {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "Type" => {
                    if value == "task" {
                        message_type = MessageType::Task;
                    }
                }
                "Depth" => {
                    if let Ok(d) = value.parse() {
                        depth = d;
                    }
                }
                "Retry-Count" => {
                    if let Ok(r) = value.parse() {
                        custom_retry = Some(r);
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
        retry_count: custom_retry.unwrap_or(retry_count),
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
        let reclaimed = adapter.pop().await.unwrap().expect("expected requeued message");
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
        };

        let custom = headers_to_custom(&headers);
        let restored = custom_to_headers(&custom, &Some("abc-123".to_string()), 0);

        assert_eq!(restored.correlation_id, "abc-123");
        assert_eq!(restored.depth, 3);
        assert_eq!(restored.retry_count, 1);
        assert!(matches!(restored.message_type, MessageType::Task));
    }
}
