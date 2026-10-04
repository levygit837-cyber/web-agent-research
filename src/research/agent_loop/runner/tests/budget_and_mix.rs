//! Turn-budget exhaustion, context growth across turns, and turns that mix
//! successful and failed calls.

use super::helpers::{
    canned_fetch, canned_search, final_answer, gateway_at, loop_input, spawn_double, stub_tools,
    text_body, tool_call, tools_body,
};
use crate::llm::ToolChoice;
use crate::research::agent_loop::result::{FailureKind, ToolResult};
use crate::research::agent_loop::runner::run_loop;
use crate::research::agent_loop::types::{LoopBudget, LoopError};
use crate::research::prompt::build_system_prompt;

#[tokio::test]
async fn budget_exhausted_aborts() {
    let tool_turn = tools_body(vec![tool_call("c1", "search", r#"{"query": "a"}"#)], "");
    let double = spawn_double(vec![(200, tool_turn.clone()), (200, tool_turn)]);
    let err = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_search(), canned_search()]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_turns: 2,
            ..LoopBudget::default()
        },
    )
    .await
    .expect_err("no FINAL within budget must abort");
    match err {
        LoopError::BudgetExhausted { turns } => assert_eq!(turns, 2),
        other => panic!("expected BudgetExhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn context_grows_across_turns() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(vec![tool_call("c1", "search", r#"{"query": "a"}"#)], ""),
        ),
        (
            200,
            tools_body(
                vec![tool_call("c2", "search", r#"{"query": "b"}"#)],
                "still looking",
            ),
        ),
        (200, text_body(&final_answer())),
    ]);
    let gateway = gateway_at(double.base());
    let tools = stub_tools(vec![canned_search(), canned_search()]);
    let input = loop_input(&["search"]);
    let budget = LoopBudget::default();
    let roster: Vec<(&str, &str)> = tools.tool_purposes();
    let system_prompt = build_system_prompt(&roster, input.size, &budget);
    let defs = tools.tool_defs(&input.allowed_tools);
    let mut history: Vec<crate::research::agent_loop::context::HistoryEntry> = Vec::new();
    let mut lengths = Vec::new();
    for _ in 1..=3 {
        let messages = crate::research::agent_loop::context::assemble(
            &system_prompt,
            &input,
            &history,
            &budget,
        );
        lengths.push(messages.len());
        let reply = if defs.is_empty() {
            gateway.chat(&messages).await.expect("double must answer")
        } else {
            gateway
                .chat_with_tools(&messages, &defs, Some(ToolChoice::Auto))
                .await
                .expect("double must answer")
        };
        if reply.tool_calls.is_empty() {
            break;
        }
        let results = reply
            .tool_calls
            .iter()
            .map(|call| crate::research::agent_loop::context::ToolMessage {
                id: call.id.clone(),
                content: "ok".to_owned(),
            })
            .collect();
        history.push(
            crate::research::agent_loop::context::HistoryEntry::ToolTurn {
                text: reply.output,
                calls: reply.tool_calls,
                results,
                replay: reply.replay,
            },
        );
    }
    assert_eq!(lengths.len(), 3);
    assert!(lengths[0] < lengths[1], "context must grow: {lengths:?}");
    assert!(lengths[1] < lengths[2], "context must grow: {lengths:?}");
}

#[tokio::test]
async fn partial_execution_failure_continues_with_next_call() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(
                vec![
                    tool_call("c1", "search", r#"{"query": "a"}"#),
                    tool_call("c2", "search", r#"{"query": "b"}"#),
                ],
                "",
            ),
        ),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![
            ToolResult::Failed {
                tool: "search".to_owned(),
                reason: "boom".to_owned(),
                kind: FailureKind::Execution,
            },
            canned_search(),
        ]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("execution failure must not stop sibling calls");
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.evidence.len(), 2);
    assert_eq!(report.failures_used, 1);
    assert_eq!(report.evidence[0].tool, "search");
}

#[tokio::test]
async fn mixed_success_resets_failure_streak() {
    let failing = tools_body(vec![tool_call("c1", "search", "{}")], "");
    let mixed = tools_body(
        vec![
            tool_call("c1", "search", r#"{"query": "a"}"#),
            tool_call("c2", "search", r#"{"query": "b"}"#),
        ],
        "",
    );
    let double = spawn_double(vec![
        (200, failing),
        (200, mixed),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![
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
            canned_search(),
        ]),
        &loop_input(&["search"]),
        &LoopBudget {
            max_repairs: 1,
            ..LoopBudget::default()
        },
    )
    .await
    .expect("a later success must reset the consecutive counter");
    assert_eq!(report.turns_used, 3);
}

#[tokio::test]
async fn unknown_tool_after_success_still_repairs() {
    let double = spawn_double(vec![
        (
            200,
            tools_body(
                vec![tool_call(
                    "c1",
                    "fetch",
                    r#"{"url": "https://example.com/a"}"#,
                )],
                "",
            ),
        ),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_fetch()]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("call outside allowed_tools must repair, not execute");
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.failures_used, 1);
    assert!(report.evidence.is_empty());
}
