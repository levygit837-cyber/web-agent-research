//! Runs that end before or at the first gateway call: pre-flight
//! `NoStart` rejections and final gateway errors (auth, status, refusal).

use std::time::Duration;

use super::helpers::{gateway_at, loop_input, refusal_body, spawn_double, stub_tools, tool_call};
use crate::llm::{Gateway, GatewayConfig, GatewayError};
use crate::research::agent_loop::runner::run_loop;
use crate::research::agent_loop::types::{LoopBudget, LoopError, LoopInput};
use crate::research::synthesis::SynthesisSize;

#[tokio::test]
async fn rejects_empty_goal() {
    let double = spawn_double(vec![]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &LoopInput {
            goal: "   ".to_owned(),
            size: SynthesisSize::Medium,
            allowed_tools: vec![],
        },
        &LoopBudget::default(),
    )
    .await
    .expect_err("empty goal must not start");
    assert!(matches!(err, LoopError::NoStart(_)));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(double.requests().is_empty(), "no I/O on NoStart");
}

#[tokio::test]
async fn unknown_tool_is_no_start() {
    let double = spawn_double(vec![]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["nope"]),
        &LoopBudget::default(),
    )
    .await
    .expect_err("unknown allowed tool must not start");
    match err {
        LoopError::NoStart(reason) => assert!(reason.contains("unknown tool: nope")),
        other => panic!("expected NoStart, got {other:?}"),
    }
}

#[tokio::test]
async fn zero_turns_and_zero_context_are_no_start() {
    let double = spawn_double(vec![]);
    let gateway = gateway_at(double.base());
    let tools = stub_tools(vec![]);
    let err = run_loop(
        &gateway,
        &tools,
        &loop_input(&[]),
        &LoopBudget {
            max_turns: 0,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("max_turns == 0 must not start");
    assert!(matches!(err, LoopError::NoStart(_)));
    let err = run_loop(
        &gateway,
        &tools,
        &loop_input(&[]),
        &LoopBudget {
            max_context_chars: 0,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("max_context_chars == 0 must not start");
    assert!(matches!(err, LoopError::NoStart(_)));
}

#[tokio::test]
async fn no_start_without_key() {
    let gateway = Gateway::new(GatewayConfig::new(
        "http://127.0.0.1:1".to_owned(),
        String::new(),
        "m".to_owned(),
    ));
    let err = run_loop(
        &gateway,
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect_err("missing key must map to NoStart");
    assert!(matches!(err, LoopError::NoStart(_)));
}

#[tokio::test]
async fn auth_aborts() {
    let double = spawn_double(vec![(401, "{}".to_owned())]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect_err("auth must abort");
    assert!(matches!(err, LoopError::Gateway(GatewayError::Auth)));
}

#[tokio::test]
async fn retryable_exhausted_aborts() {
    let double = spawn_double(vec![(500, "{}".to_owned())]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect_err("exhausted 500 must abort");
    assert!(matches!(
        err,
        LoopError::Gateway(GatewayError::Server { status: 500, .. })
    ));
}

#[tokio::test]
async fn client_error_aborts() {
    let double = spawn_double(vec![(404, "{}".to_owned())]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect_err("4xx must abort");
    assert!(matches!(
        err,
        LoopError::Gateway(GatewayError::Client { status: 404, .. })
    ));
}

#[tokio::test]
async fn refusal_aborts() {
    let double = spawn_double(vec![(200, refusal_body("nope"))]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect_err("refusal must abort");
    assert!(matches!(err, LoopError::Gateway(GatewayError::Refused(_))));
}

#[tokio::test]
async fn refusal_with_tools_aborts() {
    let body = serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "",
                "refusal": "nope",
                "tool_calls": [tool_call("c1", "search", "{}")]
            },
            "finish_reason": "stop"
        }]
    })
    .to_string();
    let double = spawn_double(vec![(200, body)]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect_err("refusal must win over tool calls");
    assert!(matches!(err, LoopError::Gateway(GatewayError::Refused(_))));
}
