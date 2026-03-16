use std::sync::Arc;
use tokio::sync::RwLock;

use apex_core::context::{MessageComposer, TokenEstimator};

// Re-export from apex-core for convenience
pub use apex_core::{now_unix_ts, summarize_json};

/// Builds a MessageComposer from the shared token estimator.
pub async fn composer_from_estimator(estimator: &Arc<RwLock<TokenEstimator>>) -> MessageComposer {
    let cal = {
        let est = estimator.read().await;
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
