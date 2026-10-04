//! Shared fixtures for `runner::tests`' themed test files: gateway reply
//! bodies, a canned gateway server, the stub tool registry, and the wire
//! body reader.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::llm::{Gateway, GatewayConfig};
use crate::research::agent_loop::registry::ToolRegistry;
use crate::research::agent_loop::result::ToolResult;
use crate::research::agent_loop::types::LoopInput;
use crate::research::synthesis::SynthesisSize;
use crate::test_support::{CannedResponse, CannedServer};
use crate::web::fetch::Evidence;

pub(super) use crate::research::agent_loop::test_support::{text_body, tool_call, tools_body};

pub(super) fn empty_body() -> String {
    serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "content": ""},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string()
}

pub(super) fn refusal_body(text: &str) -> String {
    serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "content": "", "refusal": text},
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

/// Serve each `(status, body)` in order over plain HTTP; the server's
/// `.base()` is the gateway base URL and `.requests()` the captured raw
/// requests (headers and body).
pub(super) fn spawn_double(responses: Vec<(u16, String)>) -> CannedServer {
    CannedServer::spawn(
        responses
            .into_iter()
            .map(|(status, body)| CannedResponse::text(status, body))
            .collect(),
    )
}

pub(super) fn gateway_at(base_url: &str) -> Gateway {
    Gateway::with_client(
        GatewayConfig::new(base_url.to_owned(), "test-key".to_owned(), "m".to_owned())
            .with_max_attempts(1),
        reqwest::Client::new(),
    )
}

pub(super) fn stub_tools(results: Vec<ToolResult>) -> ToolRegistry {
    ToolRegistry::stub(VecDeque::from(results))
}

pub(super) fn canned_search() -> ToolResult {
    ToolResult::Search {
        hits: vec![crate::research::agent_loop::test_support::test_hit(
            "T",
            "https://example.com/t",
        )],
    }
}

pub(super) fn canned_fetch() -> ToolResult {
    ToolResult::Fetch {
        evidence: Arc::new(Evidence::new(
            "https://example.com/t".to_owned(),
            "Body".to_owned(),
        )),
        span: 0..4,
        part: 1,
        parts: 1,
        first_delivery: true,
    }
}

pub(super) fn loop_input(allowed: &[&str]) -> LoopInput {
    LoopInput {
        goal: "What is the Obscura engine?".to_owned(),
        size: SynthesisSize::Medium,
        allowed_tools: allowed.iter().map(|name| (*name).to_owned()).collect(),
    }
}

pub(super) fn final_answer() -> String {
    "Summary with [evidence](https://example.com/t).\n\n## Findings\n\n- Point one.\n".to_owned()
}

pub(super) fn recorded_wire_bodies(double: &CannedServer) -> Vec<serde_json::Value> {
    double
        .requests()
        .iter()
        .map(|raw| {
            let (_, body) = raw
                .split_once("\r\n\r\n")
                .expect("recorded request must carry a JSON body");
            serde_json::from_str(body).expect("wire body must be JSON")
        })
        .collect()
}
