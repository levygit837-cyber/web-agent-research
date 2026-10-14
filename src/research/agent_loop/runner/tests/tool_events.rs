//! Failure and truncation Events of the loop (#148, ADR-0010 §3): the
//! `tool.failed` rows for an unknown tool, a timeout and an empty answer,
//! and `context.truncated`. The success paths are covered end to end in
//! `run::tests::tool_events`.

use std::time::Duration;

use serde_json::Value;

use super::helpers::{
    canned_search, empty_body, final_answer, gateway_at, loop_input, spawn_double, stub_tools,
    text_body, tool_call, tools_body,
};
use crate::research::agent_loop::result::ToolResult;
use crate::research::agent_loop::runner::run_loop_with_events;
use crate::research::agent_loop::types::LoopBudget;
use crate::research::events::{Events, EventsSink};

/// Run the loop against `double` with an NDJSON sink and return every
/// Event emitted, in order.
async fn run_observed(
    responses: Vec<(u16, String)>,
    tools: Vec<ToolResult>,
    budget: LoopBudget,
) -> Vec<Value> {
    let double = spawn_double(responses);
    // A counter, not a clock: parallel tests must never share a file (the
    // sink appends).
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "war-tool-events-{}-{}.ndjson",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let events = Events::open("loop-test", Some(&EventsSink::File(path.clone()))).expect("open");
    run_loop_with_events(
        &gateway_at(double.base()),
        &stub_tools(tools),
        &loop_input(&["search"]),
        &budget,
        &events,
    )
    .await
    .expect("the loop recovers");
    let text = std::fs::read_to_string(&path).expect("events file");
    let _ = std::fs::remove_file(&path);
    text.lines()
        .map(|line| serde_json::from_str(line).expect("envelope"))
        .collect()
}

fn of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == kind).collect()
}

#[tokio::test]
async fn an_unknown_tool_is_a_dispatch_failure_with_the_repair_counter() {
    let events = run_observed(
        vec![
            (200, tools_body(vec![tool_call("c1", "nope", "{}")], "")),
            (200, text_body(&final_answer())),
        ],
        vec![],
        LoopBudget::default(),
    )
    .await;

    let failed = of(&events, "tool.failed");
    assert_eq!(failed.len(), 1);
    let data = &failed[0]["data"];
    assert_eq!(failed[0]["turn"], 1);
    assert_eq!(data["call_id"], "c1");
    assert_eq!(data["tool"], "nope");
    assert_eq!(data["kind"], "dispatch");
    assert!(data["reason"].as_str().is_some_and(|r| r.contains("nope")));
    assert_eq!(data["consecutive_failures"], 1);
    assert_eq!(data["max_repairs"], 2);
    assert!(
        of(&events, "search.started").is_empty() && of(&events, "fetch.started").is_empty(),
        "an unknown tool never reaches a search or fetch Event"
    );
}

#[tokio::test]
async fn a_timed_out_search_finishes_not_ok_and_fails_as_execution() {
    let events = run_observed(
        vec![
            (
                200,
                tools_body(vec![tool_call("c1", "search", r#"{"queries": ["a"]}"#)], ""),
            ),
            (200, text_body(&final_answer())),
        ],
        vec![ToolResult::Hang],
        LoopBudget {
            tool_timeout: Duration::from_millis(50),
            ..LoopBudget::default()
        },
    )
    .await;

    let finished = of(&events, "search.finished")[0];
    assert_eq!(finished["data"]["call_id"], "c1");
    assert_eq!(finished["data"]["ok"], false);
    assert!(finished["data"]["error"]
        .as_str()
        .is_some_and(|e| e.starts_with("timeout")));
    let failed = of(&events, "tool.failed")[0];
    assert_eq!(failed["data"]["call_id"], "c1");
    assert_eq!(failed["data"]["kind"], "execution");
    assert_eq!(failed["data"]["consecutive_failures"], 1);
}

#[tokio::test]
async fn an_empty_answer_repair_is_a_tool_failed_without_a_call() {
    let events = run_observed(
        vec![(200, empty_body()), (200, text_body(&final_answer()))],
        vec![],
        LoopBudget::default(),
    )
    .await;

    let failed = of(&events, "tool.failed");
    assert_eq!(failed.len(), 1);
    let data = &failed[0]["data"];
    assert_eq!(failed[0]["turn"], 1);
    assert_eq!(data["call_id"], Value::Null);
    assert_eq!(data["tool"], Value::Null);
    assert_eq!(data["kind"], "empty_answer");
    assert_eq!(data["consecutive_failures"], 1);
    assert_eq!(data["max_repairs"], 2);
}

#[tokio::test]
async fn a_success_resets_the_counter_the_next_failure_reports() {
    let events = run_observed(
        vec![
            (200, tools_body(vec![tool_call("c1", "nope", "{}")], "")),
            (
                200,
                tools_body(vec![tool_call("c2", "search", r#"{"queries": ["a"]}"#)], ""),
            ),
            (200, tools_body(vec![tool_call("c3", "nope", "{}")], "")),
            (200, text_body(&final_answer())),
        ],
        vec![canned_search()],
        LoopBudget::default(),
    )
    .await;

    let counters: Vec<u64> = of(&events, "tool.failed")
        .iter()
        .map(|e| e["data"]["consecutive_failures"].as_u64().expect("count"))
        .collect();
    assert_eq!(counters, [1, 1], "the search in between reset the streak");
}

#[tokio::test]
async fn truncating_history_emits_context_truncated_with_the_cap() {
    let mut responses: Vec<(u16, String)> = (1..=3)
        .map(|i| {
            (
                200,
                tools_body(
                    vec![tool_call(
                        &format!("c{i}"),
                        "search",
                        r#"{"queries": ["x"]}"#,
                    )],
                    "",
                ),
            )
        })
        .collect();
    responses.push((200, text_body(&final_answer())));
    let events = run_observed(
        responses,
        vec![canned_search(), canned_search(), canned_search()],
        LoopBudget {
            max_context_chars: 400,
            ..LoopBudget::default()
        },
    )
    .await;

    let truncated = of(&events, "context.truncated");
    assert!(!truncated.is_empty(), "a 400-char budget must truncate");
    for event in &truncated {
        assert!(event["turn"].as_u64().is_some_and(|t| t >= 2));
        assert!(event["data"]["dropped_turns"]
            .as_u64()
            .is_some_and(|n| n >= 1));
        assert_eq!(event["data"]["max_context_chars"], 400);
    }
}

/// A gateway that sends a call without an id gets one synthesized; the
/// `model.replied` Event and the tool Events must name the call the same
/// way, or a Harness cannot pair them.
#[tokio::test]
async fn a_call_without_an_id_has_the_same_synthesized_id_everywhere() {
    let events = run_observed(
        vec![
            (200, tools_body(vec![tool_call("", "nope", "{}")], "")),
            (200, text_body(&final_answer())),
        ],
        vec![],
        LoopBudget::default(),
    )
    .await;

    let replied = of(&events, "model.replied")[0];
    let failed = of(&events, "tool.failed")[0];
    assert_eq!(replied["data"]["tool_calls"][0]["id"], "call_1_1");
    assert_eq!(failed["data"]["call_id"], "call_1_1");
}
