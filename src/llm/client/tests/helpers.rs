//! Shared fixtures for `client::tests`' themed test files: a canned
//! `200 OK` body, server spawning, wire-body parsing, and the two
//! `Gateway` constructors most themes reach for.

use super::super::*;
use crate::test_support::{CannedResponse, CannedServer};

pub(super) const SUCCESS_BODY: &str = r#"{
    "choices": [{
        "message": {"role": "assistant", "content": "gateway ok"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
}"#;

pub(super) fn ok() -> CannedResponse {
    CannedResponse::text(200, SUCCESS_BODY)
}

/// Spawn a server, return the base URL (no trailing slash) and the
/// server itself (its `.hits()`/`.requests()` back the old
/// `Arc<AtomicUsize>`/`Arc<Mutex<Vec<String>>>` return shapes).
pub(super) fn spawn_server(responses: Vec<CannedResponse>) -> (String, CannedServer) {
    let server = CannedServer::spawn(responses);
    let base = server.base().to_owned();
    (base, server)
}

pub(super) fn wire_body_of(request: &str) -> serde_json::Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request must carry a body");
    serde_json::from_str(body).expect("wire body must be JSON")
}

pub(super) fn messages() -> Vec<ChatMessage> {
    vec![ChatMessage::user("Reply with exactly: gateway ok")]
}

pub(super) fn single_attempt_gateway(base_url: String) -> Gateway {
    Gateway::with_client(
        GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned()).with_max_attempts(1),
        reqwest::Client::new(),
    )
}

pub(super) fn get_time_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "get_time".to_owned(),
        description: "Returns the current time.".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }]
}
