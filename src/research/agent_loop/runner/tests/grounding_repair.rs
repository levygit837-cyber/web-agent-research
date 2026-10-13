//! The grounding repair turn (#125): when it runs, what it costs in turns,
//! and which of the two answers the run keeps.

use std::time::Duration;

use tokio::time::Instant;

use super::helpers::{
    canned_fetch, gateway_at, loop_input, recorded_wire_bodies, spawn_double, stub_tools,
    text_body, tool_call, tools_body,
};
use crate::research::agent_loop::runner::run_loop;
use crate::research::agent_loop::types::{LoopBudget, RunReport};
use crate::research::synthesis::Support;
use crate::test_support::{CannedResponse, CannedServer};

const CITE: &str = "([page](https://example.com/t))";

fn fetch_turn() -> String {
    tools_body(
        vec![tool_call(
            "c1",
            "fetch",
            r#"{"url": "https://example.com/t"}"#,
        )],
        "",
    )
}

/// An answer whose bullets are `(text, quote)`; the fetched page is `Body`.
fn answer(bullets: &[(&str, Option<&str>)]) -> String {
    let mut text = format!("<answer>\nSummary {CITE}.\n\n## Findings\n");
    for (bullet, quote) in bullets {
        let quote = quote
            .map(|quote| format!(" (quote: \"{quote}\")"))
            .unwrap_or_default();
        text.push_str(&format!("- {bullet} {CITE}{quote}.\n"));
    }
    text.push_str("</answer>");
    text
}

async fn run(bodies: Vec<String>, budget: LoopBudget) -> (RunReport, CannedServer) {
    let double = spawn_double(bodies.into_iter().map(|body| (200, body)).collect());
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_fetch()]),
        &loop_input(&["fetch"]),
        &budget,
    )
    .await
    .expect("run answers");
    (report, double)
}

fn supports(report: &RunReport) -> Vec<Support> {
    report.synthesis.themes[0]
        .points
        .iter()
        .map(|point| point.support)
        .collect()
}

/// A flagged bullet triggers one tools-off turn that carries the check
/// result; the repaired answer is better, so it is kept, and the turn counts.
#[tokio::test]
async fn flagged_bullet_is_repaired_with_tools_off_and_counted() {
    let (report, double) = run(
        vec![
            fetch_turn(),
            text_body(&answer(&[("The page says Body", None)])),
            text_body(&answer(&[("The page says Body", Some("Body"))])),
        ],
        LoopBudget::default(),
    )
    .await;
    assert_eq!(report.turns_used, 3);
    assert_eq!(supports(&report), vec![Support::Exact]);
    assert_eq!(report.synthesis.verification.exact, 1);
    assert_eq!(report.usage.total_tokens, 6);
    let bodies = recorded_wire_bodies(&double);
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[2]["tool_choice"], "none");
    assert!(bodies[2]["tools"].is_array(), "same defs keep the prefix");
    let last = bodies[2]["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .expect("repair message");
    assert_eq!(last["role"], "user");
    let content = last["content"].as_str().expect("text content");
    assert!(
        content.contains("1. [Findings] The page says Body"),
        "{content}"
    );
    assert!(content.contains("quote"), "{content}");
}

/// A fully supported answer costs no extra turn.
#[tokio::test]
async fn supported_answer_makes_no_repair_turn() {
    let (report, double) = run(
        vec![
            fetch_turn(),
            text_body(&answer(&[("The page says Body", Some("Body"))])),
        ],
        LoopBudget::default(),
    )
    .await;
    assert_eq!(report.turns_used, 2);
    assert_eq!(double.requests().len(), 2);
}

/// The repaired answer loses a supported bullet: the original is kept, and
/// the turn is still counted.
#[tokio::test]
async fn worse_repair_keeps_the_original_answer() {
    let original = answer(&[
        ("The page says Body", Some("Body")),
        ("An unquoted claim", None),
    ]);
    let (report, _) = run(
        vec![
            fetch_turn(),
            text_body(&original),
            text_body(&answer(&[("An unquoted claim", None)])),
        ],
        LoopBudget::default(),
    )
    .await;
    assert_eq!(report.turns_used, 3);
    assert_eq!(supports(&report), vec![Support::Exact, Support::None]);
}

/// A repair that deletes the flagged bullet is rejected by the guard: the
/// original stays, and the turn is still counted.
#[tokio::test]
async fn repair_that_drops_a_bullet_is_rejected() {
    let original = answer(&[
        ("The page says Body", Some("Body")),
        ("An unquoted claim", None),
    ]);
    let (report, _) = run(
        vec![
            fetch_turn(),
            text_body(&original),
            text_body(&answer(&[("The page says Body", Some("Body"))])),
        ],
        LoopBudget::default(),
    )
    .await;
    assert_eq!(report.turns_used, 3);
    assert_eq!(supports(&report), vec![Support::Exact, Support::None]);
}

/// A repair that rewrites the claim while fixing the quote is rejected.
#[tokio::test]
async fn repair_that_rewrites_the_claim_is_rejected() {
    let (report, _) = run(
        vec![
            fetch_turn(),
            text_body(&answer(&[("An unquoted claim", None)])),
            text_body(&answer(&[("A different claim", Some("Body"))])),
        ],
        LoopBudget::default(),
    )
    .await;
    assert_eq!(report.turns_used, 3);
    assert_eq!(supports(&report), vec![Support::None]);
    assert!(
        report.synthesis.themes[0].points[0]
            .text
            .starts_with("An unquoted claim"),
        "the original wording stays"
    );
}

/// A repair that fixes one quote and drops the link of another bullet keeps
/// both bullets' text; the dropped-link bullet is then uncited.
#[tokio::test]
async fn repair_that_swaps_a_quote_and_drops_a_link_is_kept() {
    let (report, _) = run(
        vec![
            fetch_turn(),
            text_body(&answer(&[
                ("The page says Body", Some("Not on the page")),
                ("An unquoted claim", Some("Also not on the page")),
            ])),
            text_body(&format!(
                "<answer>\nSummary {CITE}.\n\n## Findings\n- The page says Body {CITE} (quote: \"Body\").\n- An unquoted claim.\n</answer>"
            )),
        ],
        LoopBudget::default(),
    )
    .await;
    assert_eq!(report.turns_used, 3);
    assert_eq!(supports(&report), vec![Support::Exact, Support::Unchecked]);
    assert_eq!(report.synthesis.verification.bullets, 2);
    assert_eq!(report.synthesis.verification.cited, 1);
}

/// Bullets with no link are not flagged, so the repair has nothing to do.
#[tokio::test]
async fn uncited_bullets_alone_make_no_repair_turn() {
    let (report, double) = run(
        vec![
            fetch_turn(),
            text_body(&format!(
                "<answer>\nSummary {CITE}.\n\n## Findings\n- An uncited claim.\n</answer>"
            )),
        ],
        LoopBudget::default(),
    )
    .await;
    assert_eq!(report.turns_used, 2);
    assert_eq!(double.requests().len(), 2);
}

/// Tool calls in the repair reply are not run; the original stays.
#[tokio::test]
async fn repair_reply_with_tool_calls_keeps_the_original_answer() {
    let (report, _) = run(
        vec![
            fetch_turn(),
            text_body(&answer(&[("An unquoted claim", None)])),
            fetch_turn(),
        ],
        LoopBudget::default(),
    )
    .await;
    assert_eq!(report.turns_used, 3);
    assert_eq!(supports(&report), vec![Support::None]);
    assert_eq!(report.evidence.len(), 1, "the repair turn ran no tool");
}

/// A gateway error on the repair turn keeps the original answer and is not
/// charged as a turn.
#[tokio::test]
async fn repair_gateway_error_keeps_the_original_answer() {
    let double = spawn_double(vec![
        (200, fetch_turn()),
        (200, text_body(&answer(&[("An unquoted claim", None)]))),
        (500, "{}".to_owned()),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_fetch()]),
        &loop_input(&["fetch"]),
        &LoopBudget::default(),
    )
    .await
    .expect("original answer survives");
    assert_eq!(report.turns_used, 2);
    assert_eq!(supports(&report), vec![Support::None]);
}

/// The answer wrote the last turn of the budget: no turn is left to repair.
#[tokio::test]
async fn no_repair_when_the_turn_budget_is_used() {
    let (report, double) = run(
        vec![
            fetch_turn(),
            text_body(&answer(&[("An unquoted claim", None)])),
        ],
        LoopBudget {
            max_turns: 2,
            max_turns_cap: 2,
            ..LoopBudget::default()
        },
    )
    .await;
    assert_eq!(report.turns_used, 2);
    assert_eq!(report.turn_budget, 2);
    assert_eq!(double.requests().len(), 2);
    assert_eq!(supports(&report), vec![Support::None]);
}

/// The answer arrives inside the deadline's final reserve: the repair would
/// eat the time kept for the answer, so it is skipped.
#[tokio::test]
async fn no_repair_inside_the_final_reserve() {
    let double = CannedServer::spawn(vec![
        CannedResponse::text(200, fetch_turn()),
        CannedResponse::text(200, text_body(&answer(&[("An unquoted claim", None)])))
            .with_delay(Duration::from_millis(700)),
    ]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![canned_fetch()]),
        &loop_input(&["fetch"]),
        &LoopBudget {
            deadline: Some(Instant::now() + Duration::from_millis(1_500)),
            final_reserve: Duration::from_millis(1_000),
            ..LoopBudget::default()
        },
    )
    .await
    .expect("answer before the deadline");
    assert_eq!(report.turns_used, 2);
    assert_eq!(double.requests().len(), 2);
}

/// A run that fetched no page has nothing to repair from.
#[tokio::test]
async fn no_repair_without_fetched_pages() {
    let double = spawn_double(vec![(
        200,
        text_body(&answer(&[("An unquoted claim", None)])),
    )]);
    let report = run_loop(
        &gateway_at(double.base()),
        &stub_tools(vec![]),
        &loop_input(&["fetch"]),
        &LoopBudget::default(),
    )
    .await
    .expect("answer");
    assert_eq!(report.turns_used, 1);
    assert_eq!(double.requests().len(), 1);
}
