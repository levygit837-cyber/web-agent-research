//! Shared fixtures for `registry::tests`' themed test files: call builders,
//! failure accessors, and the page bodies the stub servers serve.

use super::super::*;
use crate::test_support::{CannedResponse, CannedServer};

/// Part size for tests that do not care about splitting.
pub(super) const TEST_PART_CHARS: usize = 24_000;

pub(super) fn call(name: &str, arguments: &str) -> RequestedToolCall {
    RequestedToolCall {
        id: "call_1".to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

pub(super) fn reason(result: ToolResult) -> String {
    match result {
        ToolResult::Failed { reason, .. } => reason,
        other => panic!("expected Failed, got {other:?}"),
    }
}

pub(super) fn kind(result: &ToolResult) -> FailureKind {
    match result {
        ToolResult::Failed { kind, .. } => *kind,
        other => panic!("expected Failed, got {other:?}"),
    }
}

pub(super) fn page_html() -> String {
    format!(
        "<html><body><h1>Obscura</h1><p>{}</p></body></html>",
        "Obscura is a headless browser written in Rust. ".repeat(8)
    )
}

/// Last colon-separated segment of a `http://host:port` base URL.
pub(super) fn base_port(base: &str) -> &str {
    base.rsplit(':').next().unwrap_or_default()
}

/// A long page: 1,000 numbered paragraphs, about 60K chars of markdown.
pub(super) fn long_page_html() -> String {
    let paragraphs: String = (0..1_000)
        .map(|i| format!("<p>Paragraph {i:04}: filler words to make the page long enough.</p>"))
        .collect();
    format!("<html><body><h1>Long</h1>{paragraphs}</body></html>")
}

pub(super) async fn fetch_part(
    registry: &ToolRegistry,
    url: &str,
    part: Option<u32>,
) -> ToolResult {
    let args = match part {
        Some(part) => serde_json::json!({ "url": url, "part": part }),
        None => serde_json::json!({ "url": url }),
    };
    registry
        .execute(&call("fetch", &args.to_string()), TEST_PART_CHARS)
        .await
}

/// The slice of the stored page a `Fetch` result delivers.
pub(super) fn delivered(result: &ToolResult) -> &str {
    match result {
        ToolResult::Fetch { evidence, span, .. } => &evidence.markdown[span.clone()],
        other => panic!("expected Fetch, got {other:?}"),
    }
}

/// A canned gateway answering each body with `200` in order; its `.base()`
/// plus `/v1` is the gateway base URL.
pub(super) fn spawn_llm(bodies: Vec<String>) -> CannedServer {
    CannedServer::spawn(
        bodies
            .into_iter()
            .map(|body| CannedResponse::text(200, body))
            .collect(),
    )
}
