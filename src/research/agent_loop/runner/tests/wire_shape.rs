//! The request bytes the gateway sees: tool-call/tool-message pairing,
//! synthesized ids, prefix stability for prompt caching, and the turns-left
//! countdown.

use super::helpers::{
    canned_fetch, canned_search, final_answer, gateway_at, loop_input, recorded_wire_bodies,
    spawn_double, stub_tools, text_body, tool_call, tools_body,
};
use crate::research::agent_loop::result::{FailureKind, ToolResult};
use crate::research::agent_loop::runner::run_loop;
use crate::research::agent_loop::types::LoopBudget;

/// Same invariant as `context::assert_valid_tool_pairing`, checked
/// against the raw wire JSON recorded by the test double: every
/// assistant `tool_calls` id gets exactly one `role: "tool"` message
/// with a matching `tool_call_id`, and vice versa.
fn assert_valid_tool_pairing_json(messages: &[serde_json::Value]) {
    let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
    for message in messages {
        if message["role"] == "assistant" {
            if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
                for call in calls {
                    let id = call["id"]
                        .as_str()
                        .expect("tool_call id must be a string")
                        .to_owned();
                    assert!(pending.insert(id.clone()), "duplicate tool_call id {id}");
                }
            }
        } else if message["role"] == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .expect("tool message must carry tool_call_id")
                .to_owned();
            assert!(pending.remove(&id), "orphan tool message for id {id}");
        }
    }
    assert!(
        pending.is_empty(),
        "unpaired tool_calls left without a tool message: {pending:?}"
    );
}

#[tokio::test]
async fn tool_turn_wire_shape_pairs_every_call_with_a_tool_message() {
    let calls = vec![
        tool_call("c1", "search", r#"{"query": "a"}"#),
        tool_call("c2", "search", r#"{"query": "b"}"#),
        tool_call("c3", "search", r#"{"query": "c"}"#),
    ];
    let double = spawn_double(vec![
        (200, tools_body(calls, "")),
        (200, text_body(&final_answer())),
    ]);
    let budget = LoopBudget {
        max_tools_per_turn: 2,
        ..LoopBudget::default()
    };
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![
            canned_search(),
            ToolResult::Failed {
                tool: "search".to_owned(),
                reason: "boom".to_owned(),
                kind: FailureKind::Execution,
            },
        ]),
        &loop_input(&["search"]),
        &budget,
    )
    .await
    .expect("turn with a failure and a dropped call must still finalize");
    assert_eq!(report.turns_used, 2);

    let bodies = recorded_wire_bodies(&double);
    let messages = bodies[1]["messages"]
        .as_array()
        .expect("second request must carry messages");
    // system, goal, assistant tool_calls, tool x3.
    assert_eq!(messages.len(), 6);
    let assistant = &messages[2];
    assert_eq!(assistant["role"], "assistant");
    let tool_calls = assistant["tool_calls"]
        .as_array()
        .expect("assistant message must carry tool_calls");
    assert_eq!(tool_calls.len(), 3, "all requested calls, cap or not");
    assert_eq!(tool_calls[0]["id"], "c1");
    assert_eq!(tool_calls[1]["id"], "c2");
    assert_eq!(tool_calls[2]["id"], "c3");
    for call in tool_calls {
        assert_eq!(call["type"], "function");
    }

    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "c1");
    assert!(messages[3]["content"]
        .as_str()
        .unwrap()
        .contains("example.com/t"));

    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "c2");
    assert!(
        messages[4]["content"]
            .as_str()
            .unwrap()
            .starts_with("FAILED:"),
        "failed call must still get a tool message: {messages:?}"
    );

    assert_eq!(messages[5]["role"], "tool");
    assert_eq!(messages[5]["tool_call_id"], "c3");
    assert!(
        messages[5]["content"]
            .as_str()
            .unwrap()
            .contains("not executed"),
        "dropped-over-cap call must still get a tool message: {messages:?}"
    );

    assert_valid_tool_pairing_json(messages);
}

#[tokio::test]
async fn truncation_never_leaves_orphan_tool_message_or_unpaired_tool_calls() {
    let mut responses = Vec::new();
    for i in 1..=4 {
        responses.push((
            200,
            tools_body(
                vec![tool_call(&format!("c{i}"), "search", r#"{"query": "x"}"#)],
                "",
            ),
        ));
    }
    responses.push((200, text_body(&final_answer())));
    let double = spawn_double(responses);
    let budget = LoopBudget {
        // Small enough that only the newest turn ever survives, so every
        // later request is assembled from a freshly truncated history.
        max_context_chars: 400,
        ..LoopBudget::default()
    };
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![
            canned_search(),
            canned_search(),
            canned_search(),
            canned_search(),
        ]),
        &loop_input(&["search"]),
        &budget,
    )
    .await
    .expect("run under a tiny context budget must still finalize");
    assert_eq!(report.turns_used, 5);

    let bodies = recorded_wire_bodies(&double);
    assert_eq!(bodies.len(), 5, "one request per turn");
    for (turn, body) in bodies.iter().enumerate() {
        let messages = body["messages"]
            .as_array()
            .expect("every request must carry messages");
        assert_eq!(messages[0]["role"], "system", "turn {turn}");
        assert!(
            messages[1]["content"]
                .as_str()
                .unwrap()
                .contains("Research goal"),
            "turn {turn} goal message missing"
        );
        assert_valid_tool_pairing_json(messages);
    }
}

#[tokio::test]
async fn missing_call_id_gets_synthesized_and_paired() {
    let no_id_call = serde_json::json!({
        "type": "function",
        "function": {"name": "search", "arguments": r#"{"query": "a"}"#}
    });
    let double = spawn_double(vec![
        (200, tools_body(vec![no_id_call], "")),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search()]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("missing id must not block the run");
    assert_eq!(report.turns_used, 2);
    assert_eq!(
        report.evidence[0].id, "call_1_1",
        "evidence must carry the synthesized id"
    );

    let bodies = recorded_wire_bodies(&double);
    let messages = bodies[1]["messages"].as_array().unwrap();
    let assistant = &messages[2];
    let tool_calls = assistant["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(
        tool_calls[0]["id"], "call_1_1",
        "assistant tool_calls must carry the same synthesized id"
    );
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(
        messages[3]["tool_call_id"], "call_1_1",
        "tool message must pair with the synthesized id"
    );
    assert_valid_tool_pairing_json(messages);
}

#[tokio::test]
async fn stable_prefix_across_turns_for_prompt_caching() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(
                vec![tool_call("c1", "search", r#"{"query": "obscura"}"#)],
                "",
            ),
        ),
        (
            200,
            tools_body(
                vec![tool_call(
                    "c2",
                    "fetch",
                    r#"{"url": "https://example.com/t"}"#,
                )],
                "fetching now",
            ),
        ),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search(), canned_fetch()]),
        &loop_input(&["search", "fetch"]),
        &LoopBudget::default(),
    )
    .await
    .expect("three-turn run must finalize");
    assert_eq!(report.turns_used, 3);

    let bodies = recorded_wire_bodies(&double);
    assert_eq!(bodies.len(), 3);
    let tools: Vec<&serde_json::Value> = bodies.iter().map(|body| &body["tools"]).collect();
    assert_eq!(
        tools[0], tools[1],
        "tool defs must be byte-identical turn to turn"
    );
    assert_eq!(
        tools[1], tools[2],
        "tool defs must be byte-identical turn to turn"
    );

    let messages: Vec<&Vec<serde_json::Value>> = bodies
        .iter()
        .map(|body| body["messages"].as_array().expect("messages array"))
        .collect();
    assert_eq!(
        messages[0][0], messages[1][0],
        "system prompt (messages[0]) must be byte-identical turn to turn"
    );
    assert_eq!(
        messages[1][0], messages[2][0],
        "system prompt (messages[0]) must be byte-identical turn to turn"
    );

    let len0 = messages[0].len();
    assert_eq!(
        &messages[1][..len0],
        messages[0].as_slice(),
        "turn 1's messages must be a strict prefix of turn 2's"
    );
    let len1 = messages[1].len();
    assert_eq!(
        &messages[2][..len1],
        messages[1].as_slice(),
        "turn 2's messages must be a strict prefix of turn 3's"
    );
}

/// The model plans against the turns it has left (#72): after each tool
/// turn the newest tool result states the turns remaining, counting the
/// answer, so the last-turn warning lands on the request for the final
/// turn. The note is loop text, never part of the recorded Evidence.
#[tokio::test]
async fn newest_tool_result_counts_down_the_turns_left() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(
                vec![tool_call("c1", "search", r#"{"query": "obscura"}"#)],
                "",
            ),
        ),
        (
            200,
            tools_body(
                vec![tool_call(
                    "c2",
                    "fetch",
                    r#"{"url": "https://example.com/t"}"#,
                )],
                "",
            ),
        ),
        (200, text_body(&final_answer())),
    ]);
    let budget = LoopBudget {
        max_turns: 3,
        ..LoopBudget::default()
    };
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search(), canned_fetch()]),
        &loop_input(&["search", "fetch"]),
        &budget,
    )
    .await
    .expect("a run that answers on its last turn must finalize");
    assert_eq!(report.turns_used, 3);

    let newest_tool_result = |body: &serde_json::Value| -> String {
        body["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .and_then(|message| message["content"].as_str())
            .expect("a request after a tool turn carries a tool result")
            .to_owned()
    };
    let bodies = recorded_wire_bodies(&double);
    let second = newest_tool_result(&bodies[1]);
    assert!(second.contains("[2 turns left"), "turn 2 of 3: {second}");
    let last = newest_tool_result(&bodies[2]);
    assert!(last.contains("[1 turn left"), "turn 3 of 3: {last}");
    assert!(
        report
            .evidence
            .iter()
            .all(|item| !item.excerpt.contains("turns left")
                && !item.excerpt.contains("turn left")),
        "the turns-left note must not leak into recorded Evidence"
    );
}
