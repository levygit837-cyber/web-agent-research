//! Shared `#[cfg(test)]` fixtures for `agent_loop` and the tests above it
//! (ADR-0009): one Hit, and the OpenAI-shaped gateway reply bodies that the
//! runner, registry, and `run.rs` tests all script their canned gateway with.

use crate::web::search::types::MergedResult;

/// One Hit with the given title and URL.
pub(crate) fn test_hit(title: &str, url: &str) -> MergedResult {
    MergedResult {
        title: title.to_owned(),
        canonical_url: url.to_owned(),
        display_url: url.to_owned(),
        snippet: "S".to_owned(),
        providers: vec![],
        queries: vec![],
        best_rank: 0,
        hit_count: 1,
        published_date: None,
    }
}

pub(crate) fn tool_call(id: &str, name: &str, arguments: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
    })
}

pub(crate) fn text_body(text: &str) -> String {
    serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string()
}

pub(crate) fn tools_body(calls: Vec<serde_json::Value>, content: &str) -> String {
    serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "content": content, "tool_calls": calls},
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string()
}
