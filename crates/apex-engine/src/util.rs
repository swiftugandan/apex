use std::sync::Arc;
use tokio::sync::Mutex;

use apex_core::context::{MessageComposer, TokenEstimator};

/// Builds a MessageComposer from the shared token estimator.
pub async fn composer_from_estimator(estimator: &Arc<Mutex<TokenEstimator>>) -> MessageComposer {
    let cal = {
        let est = estimator.lock().await;
        est.calibration_data().clone()
    };
    MessageComposer::new(TokenEstimator::new(cal))
}

/// Extract the title from a message body by looking for markdown headings.
pub fn extract_title(body: &str) -> String {
    for line in body.lines() {
        if let Some(title) = line.strip_prefix("# Task: ") {
            return title.to_string();
        }
        if let Some(title) = line.strip_prefix("# Subtask: ") {
            return title.to_string();
        }
        if let Some(title) = line.strip_prefix("# Continuation: ") {
            return title.to_string();
        }
        if let Some(title) = line.strip_prefix("# ") {
            return title.to_string();
        }
    }
    "Untitled".to_string()
}

/// Summarize a JSON value to a max length string.
pub fn summarize_json(value: &serde_json::Value, max_len: usize) -> String {
    let s = value.to_string();
    if s.len() <= max_len {
        s
    } else {
        let truncated = apex_core::truncate_str(&s, max_len);
        format!("{truncated}…")
    }
}

/// Current time as a Unix timestamp string.
pub fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}
