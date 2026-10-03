//! Wire-body tests (#41): each new request param present when set, absent
//! when unset, through the real `Gateway` HTTP path (not just
//! `ChatRequest::to_wire_value` in isolation) via the capturing server.

use super::super::*;
use super::helpers::{messages, ok, spawn_server, wire_body_of};
use crate::test_support::CannedServer;

fn model_gateway(mut config: GatewayConfig) -> (String, CannedServer, Gateway) {
    let (base_url, requests) = spawn_server(vec![ok()]);
    config.base_url = base_url.clone();
    let config = config.with_max_attempts(1);
    let gateway = Gateway::with_client(config, reqwest::Client::new());
    (base_url, requests, gateway)
}

#[tokio::test]
async fn reasoning_effort_present_when_set_absent_when_unset() {
    let cfg = GatewayConfig::new("http://x".to_owned(), "k".to_owned(), "m".to_owned())
        .with_model_override(
            "m",
            crate::llm::config::ModelParams {
                reasoning_effort: Some(crate::llm::config::ReasoningEffort::High),
                max_tokens: None,
                thinking_budget: None,
            },
        );
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert_eq!(body["reasoning_effort"], serde_json::json!("high"));

    let cfg = GatewayConfig::new("http://x".to_owned(), "k".to_owned(), "m".to_owned());
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert!(
        body.get("reasoning_effort").is_none(),
        "unset reasoning_effort must be absent from the wire body, got {body}"
    );
}

#[tokio::test]
async fn thinking_enabled_shape_when_budget_set_absent_when_unset() {
    let cfg = GatewayConfig::new("http://x".to_owned(), "k".to_owned(), "m".to_owned())
        .with_model_override(
            "m",
            crate::llm::config::ModelParams {
                reasoning_effort: None,
                max_tokens: None,
                thinking_budget: Some(4096),
            },
        );
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert_eq!(
        body["thinking"],
        serde_json::json!({"type": "enabled", "budget_tokens": 4096})
    );

    let cfg = GatewayConfig::new("http://x".to_owned(), "k".to_owned(), "m".to_owned());
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert!(
        body.get("thinking").is_none(),
        "unset thinking budget must omit the field entirely, got {body}"
    );
}

#[tokio::test]
async fn thinking_disabled_shape_for_zero_budget() {
    let cfg = GatewayConfig::new("http://x".to_owned(), "k".to_owned(), "m".to_owned())
        .with_model_override(
            "m",
            crate::llm::config::ModelParams {
                reasoning_effort: None,
                max_tokens: None,
                thinking_budget: Some(0),
            },
        );
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
}

#[tokio::test]
async fn prompt_cache_key_present_when_set_absent_when_unset() {
    let cfg = GatewayConfig::new("http://x".to_owned(), "k".to_owned(), "m".to_owned())
        .with_prompt_cache_key("session-abc");
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert_eq!(body["prompt_cache_key"], serde_json::json!("session-abc"));

    let cfg = GatewayConfig::new("http://x".to_owned(), "k".to_owned(), "m".to_owned());
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert!(
        body.get("prompt_cache_key").is_none(),
        "unset prompt_cache_key must be absent, got {body}"
    );
}

#[tokio::test]
async fn extra_body_merges_gateway_specific_fields_end_to_end() {
    let mut cfg = GatewayConfig::new("http://x".to_owned(), "k".to_owned(), "m".to_owned());
    cfg.extra_body = Some(serde_json::json!({
        "verbosity": "low",
        "prompt_cache_retention": "24h"
    }));
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert_eq!(body["verbosity"], serde_json::json!("low"));
    assert_eq!(body["prompt_cache_retention"], serde_json::json!("24h"));
    assert_eq!(body["model"], serde_json::json!("m"));
}

#[tokio::test]
async fn extra_body_collision_typed_field_wins_on_wire() {
    let mut cfg = GatewayConfig::new(
        "http://x".to_owned(),
        "k".to_owned(),
        "real-model".to_owned(),
    );
    cfg.extra_body = Some(serde_json::json!({"model": "attacker-model"}));
    let (_, requests, gateway) = model_gateway(cfg);
    gateway.chat(&messages()).await.expect("call must succeed");
    let body = wire_body_of(&requests.requests()[0]);
    assert_eq!(
        body["model"],
        serde_json::json!("real-model"),
        "typed model must win over extra_body on the real wire, got {body}"
    );
}
