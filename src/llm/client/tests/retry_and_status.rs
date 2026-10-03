//! Status-code mapping, timeout, retry/backoff, and the pre-flight
//! missing-key check.

use std::time::Duration;

use super::super::*;
use super::helpers::{messages, ok, single_attempt_gateway, spawn_server};
use crate::test_support::CannedResponse;

type StatusCheck = fn(&GatewayError) -> bool;

#[tokio::test]
async fn status_mapping() {
    let cases: Vec<(CannedResponse, StatusCheck)> = vec![
        (CannedResponse::status(401), |e| {
            matches!(e, GatewayError::Auth) && !e.is_retryable()
        }),
        (
            CannedResponse::status(429).with_header("Retry-After", "7"),
            |e| {
                matches!(
                    e,
                    GatewayError::RateLimited {
                        retry_after_secs: Some(7)
                    }
                ) && e.is_retryable()
            },
        ),
        (CannedResponse::status(500), |e| {
            matches!(e, GatewayError::Server { status: 500 }) && e.is_retryable()
        }),
        (CannedResponse::status(404), |e| {
            matches!(e, GatewayError::Client { status: 404 }) && !e.is_retryable()
        }),
    ];
    for (canned, check) in cases {
        let status = canned.status;
        let (base_url, _) = spawn_server(vec![canned]);
        let err = single_attempt_gateway(base_url)
            .chat(&messages())
            .await
            .expect_err(&format!("HTTP {status} must be an error"));
        assert!(check(&err), "HTTP {status} mapped wrong: {err:?} ({err})");
    }
}

#[tokio::test]
async fn timeout_maps_and_retries() {
    let (base_url, _) = spawn_server(vec![ok().with_delay(Duration::from_secs(5))]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned()).with_max_attempts(1),
        reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .expect("test client builds"),
    );
    let err = gateway
        .chat(&messages())
        .await
        .expect_err("slow server must time out");
    assert!(
        matches!(err, GatewayError::Timeout) && err.is_retryable(),
        "slow server must map to retryable Timeout, got {err:?} ({err})"
    );
}

#[tokio::test]
async fn retry_then_success() {
    let (base_url, hits) = spawn_server(vec![CannedResponse::status(429), ok()]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned()).with_max_attempts(3),
        reqwest::Client::new(),
    );
    let reply = gateway
        .chat(&messages())
        .await
        .expect("second attempt must succeed");
    assert_eq!(reply.output, "gateway ok");
    assert_eq!(hits.hits(), 2);
}

#[tokio::test]
async fn no_retry_on_auth() {
    let (base_url, hits) = spawn_server(vec![CannedResponse::status(401)]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned()).with_max_attempts(3),
        reqwest::Client::new(),
    );
    let err = gateway.chat(&messages()).await.expect_err("401 must fail");
    assert!(matches!(err, GatewayError::Auth), "got {err:?} ({err})");
    // Give a would-be retry no chance to land: exactly one server hit.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hits.hits(), 1);
}

#[test]
fn backoff_shape() {
    assert_eq!(backoff_delay(1, None), Duration::from_secs(1));
    assert_eq!(backoff_delay(2, None), Duration::from_secs(2));
    assert_eq!(backoff_delay(2, Some(30)), Duration::from_secs(30));
    assert_eq!(backoff_delay(9, None), Duration::from_secs(8));
    assert_eq!(backoff_delay(1, Some(3600)), Duration::from_secs(60));
}

#[tokio::test]
async fn missing_key_never_touches_io() {
    let gateway = Gateway::new(GatewayConfig::new(
        "http://127.0.0.1:1".to_owned(),
        String::new(),
        "m".to_owned(),
    ));
    let err = gateway
        .chat(&messages())
        .await
        .expect_err("empty key must fail pre-flight");
    assert!(matches!(err, GatewayError::MissingApiKey));
}

#[tokio::test]
async fn forbidden_surfaces_the_upstream_message_and_does_not_retry() {
    let (base_url, hits) = spawn_server(vec![
        CannedResponse::text(
            403,
            r#"{"type":"error","error":{"type":"FreeTierError","message":"free tier gated"}}"#,
        ),
        ok(),
    ]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "k".to_owned(), "m".to_owned()).with_max_attempts(3),
        reqwest::Client::new(),
    );
    let err = gateway.chat(&messages()).await.expect_err("403 must fail");
    assert!(
        matches!(&err, GatewayError::Forbidden(detail) if detail == "free tier gated"),
        "got {err:?}"
    );
    assert!(!err.to_string().contains("bad API key"), "{err}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hits.hits(), 1);
}
