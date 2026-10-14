//! Streaming fragments on the status line (#154): the answer forms there, a retry
//! clears it, and only a terminal draws it.

use serde_json::json;

use super::*;
use crate::research::events::model::Channel;

fn text_delta(progress: &mut Progress, attempt: u32, text: &str) {
    progress.delta(
        1,
        &Delta {
            attempt,
            channel: Channel::Text,
            text,
            index: None,
            id: None,
            name: None,
        },
    );
}

fn args_delta(progress: &mut Progress, text: &str) {
    progress.delta(
        1,
        &Delta {
            attempt: 1,
            channel: Channel::ToolArgs,
            text,
            index: Some(0),
            id: Some("c1"),
            name: Some("search"),
        },
    );
}

/// The text on screen after the last `CR ESC[2K`: what a terminal shows.
fn on_screen(out: &Shared) -> String {
    out.text()
        .rsplit(CLEAR_LINE)
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn the_answer_forms_on_the_status_line_and_never_on_a_permanent_line() {
    let (mut progress, out) = tty(false);
    turn_started(&mut progress, 1, 100);
    text_delta(&mut progress, 1, "Tokio is ");
    text_delta(&mut progress, 1, "a runtime");

    assert_eq!(
        on_screen(&out),
        "turn 1/10: model writing: Tokio is a runtime"
    );
    assert!(
        !out.text().contains('\n'),
        "a fragment is not a line: {:?}",
        out.text()
    );
}

#[test]
fn a_long_answer_shows_only_its_tail_within_the_status_width() {
    let (mut progress, out) = tty(false);
    turn_started(&mut progress, 1, 100);
    for i in 0..200 {
        text_delta(&mut progress, 1, &format!("word{i} "));
    }

    let shown = on_screen(&out);
    assert!(shown.chars().count() <= STATUS_MAX_CHARS, "{shown}");
    assert!(
        shown.contains("...") && shown.ends_with("word199 "),
        "{shown}"
    );
}

#[test]
fn tool_arguments_show_a_count_not_the_json() {
    let (mut progress, out) = tty(false);
    turn_started(&mut progress, 1, 100);
    args_delta(&mut progress, r#"{"queries": ["#);
    args_delta(&mut progress, r#""tokio"]}"#);

    assert_eq!(
        on_screen(&out),
        "turn 1/10: model writing search args... 22 chars"
    );
}

#[test]
fn a_retry_drops_the_partial_and_prints_one_line() {
    let (mut progress, out) = tty(false);
    turn_started(&mut progress, 1, 100);
    text_delta(&mut progress, 1, "half an ans");
    progress.render(
        Some(1),
        "model.retry",
        900,
        &json!({"attempt": 1, "delay_ms": 800, "reason": "gateway transport error: cut mid-body"}),
    );

    let text = out.text();
    let after = text
        .rsplit(CLEAR_LINE)
        .next()
        .expect("a write after the clear");
    assert!(
        after.starts_with(
            "[turn 1/10 - 0.9s] model retry 1 in 0.8s: gateway transport error: cut mid-body\n"
        ),
        "{after:?}"
    );
    assert!(
        !after.contains("half an ans"),
        "the failed attempt's text is gone: {after:?}"
    );
    // The next attempt starts from nothing, even if it reuses the numbering.
    text_delta(&mut progress, 2, "Tokio");
    assert_eq!(on_screen(&out), "turn 1/10: model writing: Tokio");
}

#[test]
fn a_reply_ends_the_partial_so_the_next_turn_starts_clean() {
    let (mut progress, out) = tty(false);
    turn_started(&mut progress, 1, 100);
    text_delta(&mut progress, 1, "the answer");
    progress.render(
        Some(1),
        "model.replied",
        200,
        &json!({"tool_calls": [], "usage": {}}),
    );
    turn_started(&mut progress, 2, 300);
    args_delta(&mut progress, "{}");

    assert_eq!(
        on_screen(&out),
        "turn 1/10: model writing search args... 2 chars"
    );
}

#[test]
fn plain_progress_takes_no_fragments_at_all() {
    let (mut progress, out) = plain();
    assert!(!progress.wants_deltas());
    turn_started(&mut progress, 1, 100);
    let before = out.text();
    text_delta(&mut progress, 1, "secret fragment");
    args_delta(&mut progress, "{}");

    assert_eq!(out.text(), before);
}
