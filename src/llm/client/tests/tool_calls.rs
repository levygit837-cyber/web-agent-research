//! `chat_with_tools`: tool-call parsing through the real HTTP path, and
//! that an auth failure there still does not retry.

use std::time::Duration;

use super::super::*;
use super::helpers::{get_time_tools, messages, single_attempt_gateway, spawn_server};
use crate::test_support::CannedResponse;

const TOOL_CALL_BODY: &str = r#"{
    "choices": [{
        "message": {
            "role": "assistant",
            "content": "",
            "reasoning_content": "calling get_time",
            "tool_calls": [{
                "id": "chatcmpl-tool-abc123",
                "type": "function",
                "function": {"name": "get_time", "arguments": "{}"}
            }]
        },
        "finish_reason": "tool_calls"
    }],
    "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
}"#;

#[tokio::test]
async fn chat_with_tools_returns_tool_calls() {
    let (base_url, hits) = spawn_server(vec![CannedResponse::text(200, TOOL_CALL_BODY)]);
    let gateway = single_attempt_gateway(base_url);
    let reply = gateway
        .chat_with_tools(&messages(), &get_time_tools(), Some(ToolChoice::Auto))
        .await
        .expect("tool_calls body must parse");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "chatcmpl-tool-abc123");
    assert_eq!(reply.tool_calls[0].name, "get_time");
    assert_eq!(
        reply.tool_calls[0]
            .parsed_arguments()
            .expect("arguments must parse"),
        serde_json::json!({})
    );
    assert_eq!(hits.hits(), 1);
}

#[tokio::test]
async fn chat_with_tools_auth_does_not_retry() {
    let (base_url, hits) = spawn_server(vec![CannedResponse::status(401)]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned()).with_max_attempts(3),
        reqwest::Client::new(),
    );
    let err = gateway
        .chat_with_tools(&messages(), &get_time_tools(), Some(ToolChoice::Auto))
        .await
        .expect_err("401 must fail");
    assert!(matches!(err, GatewayError::Auth), "got {err:?} ({err})");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hits.hits(), 1);
}
