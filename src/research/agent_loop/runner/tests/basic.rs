//! Happy paths: a tool run reaching FINAL, citation filtering, mixed text
//! plus calls, the wire schema, the plain-`chat()` zero-tool shape, and the
//! per-turn call cap.

use std::time::Duration;

use super::helpers::{
    canned_fetch, canned_search, final_answer, gateway_at, loop_input, spawn_double, stub_tools,
    text_body, tool_call, tools_body,
};
use crate::research::agent_loop::runner::run_loop;
use crate::research::agent_loop::types::LoopBudget;
use crate::research::synthesis::SynthesisSize;

#[tokio::test]
async fn three_turn_run_to_final() {
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
    assert!(!report.synthesis.summary.is_empty());
    assert_eq!(report.synthesis.size, SynthesisSize::Medium);
    assert!(
        report
            .synthesis
            .citations
            .iter()
            .any(|citation| citation.url == "https://example.com/t"),
        "citations must contain the stub URL"
    );
    assert_eq!(report.evidence.len(), 2);
    assert_eq!(report.evidence[0].turn, 1);
    assert_eq!(report.evidence[1].turn, 2);
    assert_eq!(report.failures_used, 0);
    assert_eq!(report.usage.total_tokens, 6);
}

#[tokio::test]
async fn citations_keep_only_fetched_pages() {
    // The model links a sub-page it only saw inside fetched markdown and
    // a search Hit it never fetched; only the fetched page survives, in
    // the whole-answer set and in the theme.
    let answer = "Summary per [page](https://www.example.com/t/) and \
        [sub](https://example.com/t/sub.html).\n\n## Findings\n\n\
        - Read [page](https://example.com/t).\n\
        - Unread [hit](https://example.com/other).\n";
    let double = spawn_double(vec![
        (
            200,
            tools_body(
                vec![tool_call(
                    "c1",
                    "fetch",
                    r#"{"url": "https://example.com/t"}"#,
                )],
                "",
            ),
        ),
        (200, text_body(answer)),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_fetch()]),
        &loop_input(&["search", "fetch"]),
        &LoopBudget::default(),
    )
    .await
    .expect("run must finalize");
    let urls: Vec<&str> = report
        .synthesis
        .citations
        .iter()
        .map(|citation| citation.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec!["https://www.example.com/t/", "https://example.com/t"],
        "dedup_key variants of the fetched page stay; unfetched links go"
    );
    let theme_urls: Vec<&str> = report.synthesis.themes[0]
        .citations
        .iter()
        .map(|citation| citation.url.as_str())
        .collect();
    assert_eq!(theme_urls, vec!["https://example.com/t"]);
}

#[tokio::test]
async fn mixed_text_plus_calls_continues() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(
                vec![tool_call("c1", "search", r#"{"query": "obscura"}"#)],
                "thinking out loud",
            ),
        ),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search()]),
        &loop_input(&["search", "fetch"]),
        &LoopBudget::default(),
    )
    .await
    .expect("mixed turn must continue, not finalize");
    assert_eq!(report.turns_used, 2);
    assert!(!report.synthesis.summary.is_empty());
}

#[tokio::test]
async fn schema_injection_sends_tools_and_auto_choice() {
    let double = spawn_double(vec![(200, text_body(&final_answer()))]);
    run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["search", "fetch"]),
        &LoopBudget::default(),
    )
    .await
    .expect("zero-call run must finalize");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let bodies = double.requests();
    assert_eq!(bodies.len(), 1);
    let (_, body) = bodies[0]
        .split_once("\r\n\r\n")
        .expect("recorded request must carry a JSON body");
    let wire: serde_json::Value = serde_json::from_str(body).expect("wire body must be JSON");
    let tools = wire
        .get("tools")
        .and_then(|v| v.as_array())
        .expect("tools array");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["function"]["name"], serde_json::json!("search"));
    assert_eq!(tools[1]["function"]["name"], serde_json::json!("fetch"));
    for tool in tools {
        assert_eq!(tool["type"], serde_json::json!("function"));
        assert_eq!(
            tool["function"]["parameters"]["type"],
            serde_json::json!("object")
        );
    }
    let search = tools
        .iter()
        .find(|t| t["function"]["name"] == "search")
        .unwrap();
    assert!(search["function"]["parameters"]["required"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("queries")));
    let fetch = tools
        .iter()
        .find(|t| t["function"]["name"] == "fetch")
        .unwrap();
    assert!(fetch["function"]["parameters"]["required"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("url")));
    assert_eq!(wire.get("tool_choice"), Some(&serde_json::json!("auto")));
}

#[tokio::test]
async fn zero_tool_run_uses_plain_chat_shape() {
    let double = spawn_double(vec![(200, text_body(&final_answer()))]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&[]),
        &LoopBudget::default(),
    )
    .await
    .expect("zero-tool run must finalize");
    assert_eq!(report.turns_used, 1);
    let bodies = double.requests();
    let (_, body) = bodies[0]
        .split_once("\r\n\r\n")
        .expect("recorded request must carry a JSON body");
    let wire: serde_json::Value = serde_json::from_str(body).expect("wire body must be JSON");
    assert!(wire.get("tools").is_none(), "plain chat must omit tools");
    assert!(
        wire.get("tool_choice").is_none(),
        "plain chat must omit tool_choice"
    );
}

#[tokio::test]
async fn over_cap_calls_note_dropped_excess() {
    let calls = vec![
        tool_call("c1", "search", r#"{"query": "a"}"#),
        tool_call("c2", "search", r#"{"query": "b"}"#),
        tool_call("c3", "search", r#"{"query": "c"}"#),
        tool_call("c4", "search", r#"{"query": "d"}"#),
    ];
    let double = spawn_double(vec![
        (200, tools_body(calls, "")),
        (200, text_body(&final_answer())),
    ]);
    let budget = LoopBudget {
        max_tools_per_turn: 3,
        ..LoopBudget::default()
    };
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search(), canned_search(), canned_search()]),
        &loop_input(&["search"]),
        &budget,
    )
    .await
    .expect("over-cap turn must still finalize");
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.evidence.len(), 3);
}
