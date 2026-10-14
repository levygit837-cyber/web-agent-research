//! Anthropic Messages format (#47): real HTTP path, headers, parsing.

use super::super::*;
use super::helpers::{get_time_tools, spawn_server, wire_body_of};
use crate::test_support::CannedResponse;

const ANTHROPIC_OK: &str = r#"{
    "type": "message",
    "role": "assistant",
    "content": [
        {"type": "text", "text": "calling"},
        {"type": "tool_use", "id": "toolu_1", "name": "get_time", "input": {}}
    ],
    "stop_reason": "tool_use",
    "usage": {
        "input_tokens": 3,
        "output_tokens": 2,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 4096
    }
}"#;

#[tokio::test]
async fn anthropic_format_posts_messages_with_api_key_headers_and_parses_reply() {
    let (base_url, requests) = spawn_server(vec![CannedResponse::text(200, ANTHROPIC_OK)]);
    let mut cfg = GatewayConfig::new(base_url, "secret-key".to_owned(), "m".to_owned())
        .with_max_attempts(1)
        .with_prompt_cache_key("session-1");
    cfg.api_format = ApiFormat::Anthropic;
    let gateway = Gateway::with_client(cfg, reqwest::Client::new());
    let reply = gateway
        .chat_with_tools(
            &[ChatMessage::system("sys"), ChatMessage::user("what time?")],
            &get_time_tools(),
            Some(ToolChoice::Auto),
        )
        .await
        .expect("anthropic call must succeed");

    let raw = requests.requests()[0].clone();
    let head = raw
        .split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(head.starts_with("post /messages "), "wrong path: {head}");
    assert!(
        head.contains("x-api-key: secret-key"),
        "missing x-api-key: {head}"
    );
    assert!(
        head.contains("anthropic-version: 2023-06-01"),
        "missing version: {head}"
    );
    assert!(
        !head.contains("authorization:"),
        "must not send Bearer auth: {head}"
    );
    let body = wire_body_of(&raw);
    assert_eq!(body["system"][0]["text"], "sys");
    assert_eq!(body["tools"][0]["name"], "get_time");
    assert!(body.get("prompt_cache_key").is_none());

    assert_eq!(reply.output, "calling");
    assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "toolu_1");
    assert_eq!(reply.usage.cached_prompt_tokens, 4096);
    assert_eq!(reply.usage.prompt_tokens, 4099);
}
