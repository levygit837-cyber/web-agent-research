//! Error objects inside 2xx bodies, both wire formats: the upstream
//! message survives, overload/rate-limit/internal kinds are retried, and
//! everything else is a rejection.

use super::super::*;
use super::helpers::{messages, ok, spawn_server};
use crate::test_support::CannedResponse;

fn gateway(base_url: String, format: ApiFormat, attempts: u32) -> Gateway {
    let mut cfg =
        GatewayConfig::new(base_url, "k".to_owned(), "m".to_owned()).with_max_attempts(attempts);
    cfg.api_format = format;
    Gateway::with_client(cfg, reqwest::Client::new())
}

#[tokio::test]
async fn openai_error_in_200_keeps_the_message_and_is_not_retried() {
    let body = r#"{"error":{"message":"Unknown model: m","type":"invalid_request_error","code":"model_not_found"}}"#;
    let (base_url, server) = spawn_server(vec![CannedResponse::text(200, body), ok()]);
    let err = gateway(base_url, ApiFormat::Openai, 3)
        .chat(&messages())
        .await
        .expect_err("error body must fail");
    assert!(
        matches!(&err, GatewayError::ErrorBody { kind, message, retryable: false }
            if kind == "invalid_request_error" && message == "Unknown model: m"),
        "{err:?}"
    );
    assert!(err.to_string().contains("Unknown model: m"), "{err}");
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn openai_server_error_in_200_is_retried() {
    let body =
        r#"{"error":{"message":"The server had an error","type":"server_error"},"choices":[]}"#;
    let (base_url, server) = spawn_server(vec![CannedResponse::text(200, body), ok()]);
    let reply = gateway(base_url, ApiFormat::Openai, 3)
        .chat(&messages())
        .await
        .expect("retry must succeed");
    assert_eq!(reply.output, "gateway ok");
    assert_eq!(server.hits(), 2);
}

#[tokio::test]
async fn openai_string_error_in_200_is_a_rejection() {
    let (base_url, _) = spawn_server(vec![CannedResponse::text(200, r#"{"error":"quota gone"}"#)]);
    let err = gateway(base_url, ApiFormat::Openai, 1)
        .chat(&messages())
        .await
        .expect_err("error body must fail");
    assert!(
        matches!(&err, GatewayError::ErrorBody { message, retryable: false, .. } if message == "quota gone"),
        "{err:?}"
    );
}

#[tokio::test]
async fn anthropic_overloaded_in_200_is_retried() {
    let overloaded =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let message_ok =
        r#"{"type":"message","content":[{"type":"text","text":"fine"}],"stop_reason":"end_turn"}"#;
    let (base_url, server) = spawn_server(vec![
        CannedResponse::text(200, overloaded),
        CannedResponse::text(200, message_ok),
    ]);
    let reply = gateway(base_url, ApiFormat::Anthropic, 3)
        .chat(&messages())
        .await
        .expect("retry must succeed");
    assert_eq!(reply.output, "fine");
    assert_eq!(server.hits(), 2);
}

#[tokio::test]
async fn anthropic_invalid_request_in_200_is_a_rejection() {
    let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens too large"}}"#;
    let (base_url, server) = spawn_server(vec![CannedResponse::text(200, body)]);
    let err = gateway(base_url, ApiFormat::Anthropic, 3)
        .chat(&messages())
        .await
        .expect_err("error body must fail");
    assert!(!err.is_retryable(), "{err:?}");
    assert!(err.to_string().contains("max_tokens too large"), "{err}");
    assert_eq!(server.hits(), 1);
}
