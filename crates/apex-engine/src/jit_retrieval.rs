//! Just-in-time retrieval of long-term facts at claim start.
//!
//! Queries the memory store with a derived query (body + goal), then formats
//! the returned facts into a markdown section capped by a token budget.

use apex_core::context::{MessageComposer, TokenEstimator};
use apex_core::ports::MemoryStore;

const QUERY_MAX_CHARS: usize = 400;

/// Derive a search query from claim body and scratchpad goal for JIT fact retrieval.
/// Returns a truncated concatenation (body + goal) so FTS gets meaningful terms.
pub fn derive_query(body: &str, goal: &str) -> String {
    let combined = if goal.is_empty() {
        body.trim().to_string()
    } else if body.trim().is_empty() {
        goal.trim().to_string()
    } else {
        format!("{} {}", body.trim(), goal.trim())
    };
    let trimmed = combined.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.len() <= QUERY_MAX_CHARS {
        trimmed.to_string()
    } else {
        trimmed.chars().take(QUERY_MAX_CHARS).collect::<String>()
    }
}

/// Retrieve relevant facts from the store and format them into a markdown section
/// that fits within `max_tokens`. Returns an empty string if the query is empty,
/// the store fails, or no facts fit the budget.
pub async fn retrieve_facts_section(
    store: &dyn MemoryStore,
    estimator: TokenEstimator,
    query: &str,
    max_facts: usize,
    max_tokens: u32,
) -> String {
    if query.trim().is_empty() {
        return String::new();
    }
    let facts = match store.query_facts(query.trim(), max_facts).await {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    if facts.is_empty() {
        return String::new();
    }
    let composer = MessageComposer::new(estimator);
    composer.format_facts_section(&facts, max_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_query_empty_both() {
        assert!(derive_query("", "").is_empty());
        assert!(derive_query("  ", "  ").is_empty());
    }

    #[test]
    fn derive_query_body_only() {
        let q = derive_query("fix the bug in auth", "");
        assert_eq!(q, "fix the bug in auth");
    }

    #[test]
    fn derive_query_goal_only() {
        let q = derive_query("", "implement login");
        assert_eq!(q, "implement login");
    }

    #[test]
    fn derive_query_combined_truncated() {
        let body = "a".repeat(500);
        let goal = "goal";
        let q = derive_query(&body, goal);
        assert_eq!(q.len(), QUERY_MAX_CHARS);
        assert!(q.starts_with('a'));
    }
}
