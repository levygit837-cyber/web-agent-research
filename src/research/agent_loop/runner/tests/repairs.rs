//! Model-visible failures bounded by `max_repairs`: unknown tools, bad
//! arguments, tool errors, timeouts, and empty answers.

use std::collections::VecDeque;
use std::time::Duration;

use super::helpers::{
    empty_body, final_answer, gateway_at, loop_input, recorded_wire_bodies, spawn_double,
    stub_tools, text_body, tool_call, tools_body,
};
use crate::research::agent_loop::registry::ToolRegistry;
use crate::research::agent_loop::result::{FailureKind, ToolResult};
use crate::research::agent_loop::runner::run_loop;
use crate::research::agent_loop::types::{LoopBudget, LoopError};

#[tokio::test]
async fn unknown_tool_repairs_then_finalizes() {
    let double = spawn_double(vec![
        (200, tools_body(vec![tool_call("c1", "nope", "{}")], "")),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("one unknown tool must repair");
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.failures_used, 1);
}

#[tokio::test]
async fn unknown_tool_aborts_after_repairs() {
    let repeated = tools_body(vec![tool_call("c1", "nope", "{}")], "");
    let double = spawn_double(vec![
        (200, repeated.clone()),
        (200, repeated.clone()),
        (200, repeated),
    ]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_repairs: 2,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("unknown-tool streak must abort");
    match err {
        LoopError::InvalidToolCall { reason } => assert!(reason.contains("unknown tool")),
        other => panic!("expected InvalidToolCall, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_arguments_json_repairs() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(vec![tool_call("c1", "search", "{bad json")], ""),
        ),
        (200, text_body(&final_answer())),
    ]);
    let tools = ToolRegistry::offline();
    let report = run_loop(
        &gateway_at(double.base()),
        &tools,
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("bad JSON must repair");
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.failures_used, 1);
}

#[tokio::test]
async fn invalid_args_repair_then_abort() {
    let bad = tools_body(vec![tool_call("c1", "search", r#"{"query": ""}"#)], "");
    let double = spawn_double(vec![(200, bad.clone()), (200, bad)]);
    let tools = ToolRegistry::offline();
    let err = run_loop(
        &gateway_at(double.base()),
        &tools,
        &loop_input(&["search"]),
        &LoopBudget {
            max_repairs: 1,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("invalid-args streak must abort");
    match err {
        LoopError::InvalidToolCall { reason } => assert!(reason.contains("invalid args")),
        other => panic!("expected InvalidToolCall, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_error_is_observation_then_repair() {
    let double = spawn_double(vec![
        (200, tools_body(vec![tool_call("c1", "search", "{}")], "")),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![ToolResult::Failed {
            tool: "search".to_owned(),
            reason: "boom".to_owned(),
            kind: FailureKind::Execution,
        }]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("executor failure must repair");
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.failures_used, 1);
    assert!(report.evidence.iter().all(|item| item.tool == "search"));
}

#[tokio::test]
async fn repeated_tool_failure_aborts() {
    let failing = tools_body(vec![tool_call("c1", "search", "{}")], "");
    let double = spawn_double(vec![
        (200, failing.clone()),
        (200, failing.clone()),
        (200, failing),
    ]);
    let failing_tools = || {
        ToolRegistry::stub(VecDeque::from([
            ToolResult::Failed {
                tool: "search".to_owned(),
                reason: "boom".to_owned(),
                kind: FailureKind::Execution,
            },
            ToolResult::Failed {
                tool: "search".to_owned(),
                reason: "boom".to_owned(),
                kind: FailureKind::Execution,
            },
            ToolResult::Failed {
                tool: "search".to_owned(),
                reason: "boom".to_owned(),
                kind: FailureKind::Execution,
            },
        ]))
    };
    let err = run_loop(
        &gateway_at(double.base()),
        &failing_tools(),
        &loop_input(&["search"]),
        &LoopBudget {
            max_repairs: 2,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("execution streak must abort");
    match err {
        LoopError::ToolFailed { reason } => assert!(reason.contains("boom")),
        other => panic!("expected ToolFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_timeout_is_observation_then_repair() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(vec![tool_call("c1", "search", r#"{"query": "a"}"#)], ""),
        ),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![ToolResult::Hang]),
        &loop_input(&["search"]),
        &LoopBudget {
            tool_timeout: Duration::from_millis(50),
            ..LoopBudget::default()
        },
    )
    .await
    .expect("timeout must surface as a repairable observation");
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.failures_used, 1);
    assert!(report.evidence.iter().any(|item| item.tool == "search"));
}

/// A fetch the loop times out is a failed fetch like any other (#72):
/// its observation carries the same next step, so the model picks a
/// different Hit instead of retrying the slow URL.
#[tokio::test]
async fn timed_out_fetch_points_to_another_hit() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(
                vec![tool_call(
                    "c1",
                    "fetch",
                    r#"{"url": "https://example.com/slow"}"#,
                )],
                "",
            ),
        ),
        (200, text_body(&final_answer())),
    ]);
    run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![ToolResult::Hang]),
        &loop_input(&["search", "fetch"]),
        &LoopBudget {
            tool_timeout: Duration::from_millis(50),
            ..LoopBudget::default()
        },
    )
    .await
    .expect("a timed-out fetch is a repairable observation");

    let bodies = recorded_wire_bodies(&double);
    let observation = bodies[1]["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the timed-out call is paired with a tool result")
        .to_owned();
    let reason = observation
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("FAILED: "))
        .expect("a failed call starts with FAILED:");
    assert!(reason.starts_with("timeout"), "{observation}");
    let failed_fetch = ToolResult::Failed {
        tool: "fetch".to_owned(),
        reason: reason.to_owned(),
        kind: FailureKind::Execution,
    }
    .render();
    assert!(observation.starts_with(&failed_fetch), "{observation}");
}

#[tokio::test]
async fn repeated_timeout_aborts_as_tool_failed() {
    let hanging = tools_body(vec![tool_call("c1", "search", r#"{"query": "a"}"#)], "");
    let double = spawn_double(vec![(200, hanging.clone()), (200, hanging)]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![ToolResult::Hang, ToolResult::Hang]),
        &loop_input(&["search"]),
        &LoopBudget {
            tool_timeout: Duration::from_millis(50),
            max_repairs: 1,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("timeout streak must abort");
    match err {
        LoopError::ToolFailed { reason } => assert!(reason.contains("timeout after")),
        other => panic!("expected ToolFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn strict_repairs_zero_aborts_on_first_failure() {
    let double = spawn_double(vec![(
        200,
        tools_body(vec![tool_call("c1", "nope", "{}")], ""),
    )]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_repairs: 0,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("max_repairs 0 must abort immediately");
    assert!(matches!(err, LoopError::InvalidToolCall { .. }));
}

#[tokio::test]
async fn empty_answer_repairs() {
    let double = spawn_double(vec![(200, empty_body()), (200, text_body(&final_answer()))]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("empty answer must repair");
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.failures_used, 1);
}

/// An empty reply gets a repair turn that keeps the countdown (#72):
/// with turns to spare it lists the tools and the turns left; when only
/// the last turn is left it asks for the answer now and lists no tools,
/// since a tool call on the last turn ends the run with no answer.
#[tokio::test]
async fn empty_answer_repair_keeps_the_countdown() {
    for (max_turns, note, lists_tools) in [(8, "[7 turns left", true), (2, "[1 turn left", false)] {
        let double = spawn_double(vec![(200, empty_body()), (200, text_body(&final_answer()))]);
        let report = run_loop(
            &gateway_at(double.base()),
            &stub_tools(vec![]),
            &loop_input(&["search", "fetch"]),
            &LoopBudget {
                max_turns,
                ..LoopBudget::default()
            },
        )
        .await
        .expect("an empty reply is repaired, then the answer finalizes");
        assert_eq!(report.turns_used, 2);
        let bodies = recorded_wire_bodies(&double);
        let repair = bodies[1]["messages"]
            .as_array()
            .expect("messages array")
            .last()
            .and_then(|message| message["content"].as_str())
            .expect("the repair observation is the newest message")
            .to_owned();
        assert!(repair.contains(note), "max_turns {max_turns}: {repair}");
        assert_eq!(
            repair.contains(crate::web::search::tool::SEARCH_TOOL_PURPOSE),
            lists_tools,
            "max_turns {max_turns}: {repair}"
        );
    }
}
