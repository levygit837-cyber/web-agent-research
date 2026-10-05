//! Turn budget growth from searches, the tools-off final turn (#101), and
//! the run deadline with its final-answer reserve (#100).

use std::time::Duration;

use tokio::time::Instant;

use super::helpers::{
    canned_search, final_answer, gateway_at, loop_input, recorded_wire_bodies, spawn_double,
    stub_tools, text_body, tool_call, tools_body,
};
use crate::research::agent_loop::result::ToolResult;
use crate::research::agent_loop::runner::run_loop;
use crate::research::agent_loop::types::{LoopBudget, LoopError};

fn search_turn(id: &str) -> String {
    tools_body(vec![tool_call(id, "search", r#"{"query": "a"}"#)], "")
}

/// The second successful search extends the budget, so turn 3, which would
/// have been the tools-off final turn of a 3-turn run, still runs with
/// `auto`; the cap bounds the growth (3 + 5 capped at 6).
#[tokio::test]
async fn second_successful_search_extends_the_budget_up_to_the_cap() {
    let double = spawn_double(vec![
        (200, search_turn("c1")),
        (200, search_turn("c2")),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search(), canned_search()]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_turns: 3,
            max_turns_cap: 6,
            ..LoopBudget::default()
        },
    )
    .await
    .expect("extended budget leaves room for the answer");
    assert_eq!(report.turns_used, 3);
    assert_eq!(report.turn_budget, 6);
    let bodies = recorded_wire_bodies(&double);
    assert_eq!(bodies[2]["tool_choice"], "auto");
}

/// A failed search grants nothing: two successes and one blocked search
/// extend a 4-turn budget once, to 9.
#[tokio::test]
async fn failed_searches_do_not_extend_the_budget() {
    let double = spawn_double(vec![
        (200, search_turn("c1")),
        (200, search_turn("c2")),
        (200, search_turn("c3")),
        (200, text_body(&final_answer())),
    ]);
    let failed = ToolResult::SearchBlocked {
        detail: "challenge".to_owned(),
    };
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search(), failed, canned_search()]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_turns: 4,
            ..LoopBudget::default()
        },
    )
    .await
    .expect("answer within the extended budget");
    assert_eq!(report.turns_used, 4);
    assert_eq!(report.turn_budget, 9);
}

/// The last turn sends the same tool defs with `tool_choice: none`; tool
/// calls in its reply are not dispatched and the run ends with no answer.
#[tokio::test]
async fn final_turn_is_tools_off_and_its_tool_calls_are_not_dispatched() {
    let double = spawn_double(vec![(200, search_turn("c1")), (200, search_turn("c2"))]);
    // One queued result: dispatching the second call would hit an empty
    // queue and record a tool failure.
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search()]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_turns: 2,
            max_turns_cap: 2,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("tool calls on the final turn leave no answer");
    match err {
        LoopError::BudgetExhausted { turns } => assert_eq!(turns, 2),
        other => panic!("expected BudgetExhausted, got {other:?}"),
    }
    let bodies = recorded_wire_bodies(&double);
    assert_eq!(bodies[0]["tool_choice"], "auto");
    assert_eq!(bodies[1]["tool_choice"], "none");
    assert_eq!(
        bodies[0]["tools"], bodies[1]["tools"],
        "same defs on the final turn"
    );
    let note = bodies[1]["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(note.contains("[1 turn left: tools are off"), "{note}");
}

/// A one-turn run is its own final turn and can still answer.
#[tokio::test]
async fn a_one_turn_run_answers_on_its_tools_off_turn() {
    let double = spawn_double(vec![(200, text_body(&final_answer()))]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_turns: 1,
            ..LoopBudget::default()
        },
    )
    .await
    .expect("one-turn answer");
    assert_eq!((report.turns_used, report.turn_budget), (1, 1));
    assert_eq!(recorded_wire_bodies(&double)[0]["tool_choice"], "none");
}

#[tokio::test]
async fn cap_below_max_turns_does_not_start() {
    let double = spawn_double(vec![]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_turns: 5,
            max_turns_cap: 4,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("cap below max_turns");
    assert!(matches!(err, LoopError::NoStart(_)), "{err:?}");
    assert_eq!(double.hits(), 0);
}

/// A run whose deadline already passed makes no gateway call.
#[tokio::test]
async fn a_passed_deadline_ends_the_run_without_a_call() {
    let double = spawn_double(vec![]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &LoopBudget {
            deadline: Some(Instant::now()),
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("deadline passed");
    assert!(
        matches!(err, LoopError::DeadlineExceeded { turns: 0 }),
        "{err:?}"
    );
    assert_eq!(double.hits(), 0);
}

/// Inside the final reserve the next turn is the tools-off final turn,
/// whatever turns are left: an answer is kept, tool calls end the run with
/// `DeadlineExceeded`.
#[tokio::test]
async fn inside_the_final_reserve_the_next_turn_is_final() {
    let reserve_budget = || LoopBudget {
        deadline: Some(Instant::now() + Duration::from_secs(30)),
        final_reserve: Duration::from_secs(60),
        ..LoopBudget::default()
    };
    let answered = spawn_double(vec![(200, text_body(&final_answer()))]);
    let report = run_loop(
        &gateway_at(answered.base()),
        &stub_tools(vec![]),
        &loop_input(&["search"]),
        &reserve_budget(),
    )
    .await
    .expect("answer on the deadline's final turn");
    assert_eq!(report.turns_used, 1);
    assert_eq!(recorded_wire_bodies(&answered)[0]["tool_choice"], "none");

    let called = spawn_double(vec![(200, search_turn("c1"))]);
    let err = run_loop(
        &gateway_at(called.base()),
        &stub_tools(vec![canned_search()]),
        &loop_input(&["search"]),
        &reserve_budget(),
    )
    .await
    .expect_err("no answer on the deadline's final turn");
    assert!(
        matches!(err, LoopError::DeadlineExceeded { turns: 1 }),
        "{err:?}"
    );
}

/// A tool call is cut at the work cutoff (deadline minus the reserve), not
/// at `tool_timeout`, and the run still gets its final answer turn.
#[tokio::test]
async fn a_hung_tool_is_cut_at_the_work_cutoff() {
    let double = spawn_double(vec![
        (200, search_turn("c1")),
        (200, text_body(&final_answer())),
    ]);
    let started = std::time::Instant::now();
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![ToolResult::Hang]),
        &loop_input(&["search"]),
        &LoopBudget {
            deadline: Some(Instant::now() + Duration::from_millis(1_500)),
            final_reserve: Duration::from_millis(1_000),
            ..LoopBudget::default()
        },
    )
    .await
    .expect("final answer after the cut tool");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(report.turns_used, 2);
    let bodies = recorded_wire_bodies(&double);
    assert_eq!(bodies[1]["tool_choice"], "none");
    assert!(
        bodies[1].to_string().contains("the run deadline is near"),
        "{}",
        bodies[1]
    );
}

/// The Messages format carries the tools-off final turn as
/// `tool_choice: {"type": "none"}`, with the same tool definitions.
#[tokio::test]
async fn anthropic_final_turn_sends_tool_choice_none() {
    let anthropic = |content: serde_json::Value| {
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": content,
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string()
    };
    let double = spawn_double(vec![
        (
            200,
            anthropic(serde_json::json!([
                {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"query": "a"}}
            ])),
        ),
        (
            200,
            anthropic(serde_json::json!([{"type": "text", "text": final_answer()}])),
        ),
    ]);
    let mut config = crate::llm::GatewayConfig::new(
        double.base().to_owned(),
        "test-key".to_owned(),
        "m".to_owned(),
    )
    .with_max_attempts(1);
    config.api_format = crate::llm::ApiFormat::Anthropic;
    let gateway = crate::llm::Gateway::with_client(config, reqwest::Client::new());
    let report = run_loop(
        &gateway,
        &stub_tools(vec![canned_search()]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_turns: 2,
            ..LoopBudget::default()
        },
    )
    .await
    .expect("answer on the final turn");
    assert_eq!(report.turns_used, 2);
    let bodies = recorded_wire_bodies(&double);
    assert_eq!(
        bodies[0]["tool_choice"],
        serde_json::json!({"type": "auto"})
    );
    assert_eq!(
        bodies[1]["tool_choice"],
        serde_json::json!({"type": "none"})
    );
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
}
