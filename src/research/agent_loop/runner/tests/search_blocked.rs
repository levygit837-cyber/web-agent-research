//! `RunReport.search_blocked` and `had_search_hits` across turns (#53).

use super::helpers::{
    canned_search, final_answer, gateway_at, loop_input, spawn_double, stub_tools, text_body,
    tool_call, tools_body,
};
use crate::research::agent_loop::result::ToolResult;
use crate::research::agent_loop::runner::run_loop;
use crate::research::agent_loop::types::LoopBudget;

/// Regression (#53): a `search` call that comes back
/// `ToolResult::SearchBlocked` (every leg Challenge-walled) must survive
/// into `RunReport.search_blocked` even though the model still manages a
/// FINAL on the next turn (the failure counts as a repair, not a fatal
/// `ToolFailed`, since `max_repairs` covers it).
#[tokio::test]
async fn search_blocked_survives_a_repaired_turn_into_the_run_report() {
    let double = spawn_double(vec![
        (200, tools_body(vec![tool_call("c1", "search", "{}")], "")),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![ToolResult::SearchBlocked {
            detail: "startpage: pow challenge; duckduckgo: anomaly".to_owned(),
        }]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("a repairable SearchBlocked turn must still finalize");
    assert_eq!(report.turns_used, 2);
    assert_eq!(
        report.search_blocked.as_deref(),
        Some("startpage: pow challenge; duckduckgo: anomaly")
    );
    assert!(
        !report.had_search_hits,
        "no search call this run ever returned a Hit"
    );
}

/// A `search` call earlier in the run that did return Hits must set
/// `had_search_hits`, even if a later call in the same run comes back
/// `SearchBlocked` (a later re-plan hitting the wall on a fresh query).
#[tokio::test]
async fn had_search_hits_true_once_any_search_call_returns_hits() {
    let double = spawn_double(vec![
        (200, tools_body(vec![tool_call("c1", "search", "{}")], "")),
        (200, tools_body(vec![tool_call("c2", "search", "{}")], "")),
        (200, text_body(&final_answer())),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![
            canned_search(),
            ToolResult::SearchBlocked {
                detail: "startpage: pow challenge".to_owned(),
            },
        ]),
        &loop_input(&["search"]),
        &LoopBudget::default(),
    )
    .await
    .expect("run must finalize");
    assert_eq!(report.turns_used, 3);
    assert!(
        report.had_search_hits,
        "the first search call returned a Hit"
    );
    assert_eq!(
        report.search_blocked.as_deref(),
        Some("startpage: pow challenge")
    );
}
