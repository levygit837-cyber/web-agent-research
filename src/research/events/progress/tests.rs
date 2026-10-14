use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;

use super::*;

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().clone()).expect("utf-8")
    }
}

fn plain() -> (Progress, Shared) {
    let out = Shared::default();
    (Progress::new(Style::Plain, Box::new(out.clone())), out)
}

fn tty(color: bool) -> (Progress, Shared) {
    let out = Shared::default();
    (
        Progress::new(Style::Tty { color }, Box::new(out.clone())),
        out,
    )
}

fn turn_started(progress: &mut Progress, turn: u32, elapsed_ms: u64) {
    progress.render(
        Some(turn),
        "turn.started",
        elapsed_ms,
        &json!({"turn_budget": 10, "turns_left": 10 - turn, "final_turn": false, "final_reason": null}),
    );
}

#[test]
fn a_search_line_names_the_queries_hits_and_every_engine_state() {
    let (mut progress, out) = plain();
    turn_started(&mut progress, 2, 1000);
    progress.render(
        Some(2),
        "search.finished",
        4100,
        &json!({
            "call_id": "c1",
            "queries": ["tokio latest version", "tokio crates.io"],
            "ok": true,
            "hits": 14,
            "engines": [
                {"engine": "ddg", "status": "rows", "rows": 6, "cached": false, "problems": []},
                {"engine": "bing", "status": "rows", "rows": 5, "cached": false, "problems": []},
                {"engine": "yahoo", "status": "rows", "rows": 3, "cached": true, "problems": []},
                {"engine": "brave", "status": "suspended", "rows": 0, "cached": false,
                 "problems": [{"kind": "suspended", "reason": "challenge", "remaining_secs": 100}]},
            ],
            "note": null,
            "error": null,
        }),
    );
    assert_eq!(
        out.text(),
        "[turn 2/10 - 4.1s] search \"tokio latest version\", \"tokio crates.io\" -> 14 hits \
         (ddg 6, bing 5, yahoo 3; brave suspended 2m)\n"
    );
}

#[test]
fn a_failed_fetch_says_failed_and_why_and_a_part_shows_its_position() {
    let (mut progress, out) = plain();
    turn_started(&mut progress, 3, 0);
    progress.render(
        Some(3),
        "fetch.finished",
        5200,
        &json!({
            "call_id": "f1", "url": "https://a.example/doc", "ok": true,
            "final_url": "https://a.example/doc", "fetch_path": "static",
            "part": 1, "parts": 3, "chars": 12_345, "served_from": "network", "error": null,
        }),
    );
    progress.render(
        Some(3),
        "fetch.finished",
        5900,
        &json!({
            "call_id": "f2", "url": "https://10.0.0.1/", "ok": false,
            "final_url": null, "fetch_path": null, "part": null, "parts": null,
            "chars": 0, "served_from": null, "error": "refused: private network",
        }),
    );
    assert_eq!(
        out.text(),
        "[turn 3/10 - 5.2s] fetch https://a.example/doc -> 12.3k chars [static, part 1/3]\n\
         [turn 3/10 - 5.9s] fetch https://10.0.0.1/ -> failed: refused: private network\n"
    );
}

#[test]
fn an_error_session_ends_with_the_reason_on_the_final_line() {
    let (mut progress, out) = plain();
    progress.render(
        None,
        "session.finished",
        61_250,
        &json!({
            "outcome": "deadline_exceeded", "exit_code": 9, "turns_used": 3, "turn_budget": 10,
            "usage": {"prompt_tokens": 9000, "completion_tokens": 400, "total_tokens": 9400,
                      "reasoning_tokens": 0, "cached_prompt_tokens": 0,
                      "cache_creation_prompt_tokens": 0},
            "evidence_urls": [],
            "error": "run deadline of 60s exceeded",
        }),
    );
    assert_eq!(
        out.text(),
        "[61.2s] finished: deadline_exceeded, 3/10 turns, 9.4k tokens: run deadline of 60s exceeded\n"
    );
}

#[test]
fn a_successful_session_repeats_the_verification_counts_on_the_final_line() {
    let (mut progress, out) = plain();
    turn_started(&mut progress, 4, 0);
    progress.render(
        Some(4),
        "answer.verified",
        7000,
        &json!({"bullets": 5, "cited": 4, "exact": 3, "partial": 1, "none": 0, "citations": 4}),
    );
    progress.render(
        None,
        "session.finished",
        7100,
        &json!({
            "outcome": "ok", "exit_code": 0, "turns_used": 4, "turn_budget": 10,
            "usage": {"total_tokens": 12_000}, "evidence_urls": [], "error": null,
        }),
    );
    let text = out.text();
    let last = text.lines().last().expect("a final line");
    assert_eq!(
        last,
        "[7.1s] finished: ok, 4/10 turns, 12.0k tokens, 5 bullets: 4 cited, 3 exact, 1 partial, 0 none"
    );
}

#[test]
fn model_replies_distinguish_tool_calls_from_the_answer() {
    let (mut progress, out) = plain();
    turn_started(&mut progress, 1, 0);
    let usage = json!({"prompt_tokens": 1500, "completion_tokens": 80});
    progress.render(
        Some(1),
        "model.replied",
        900,
        &json!({"tool_calls": [{"id": "a", "name": "search"}, {"id": "b", "name": "fetch"}], "usage": usage}),
    );
    progress.render(
        Some(1),
        "model.replied",
        2000,
        &json!({"tool_calls": [], "usage": usage}),
    );
    progress.render(
        Some(1),
        "model.replied",
        2500,
        &json!({"tool_calls": [], "usage": {"prompt_tokens": 0, "completion_tokens": 42}}),
    );
    assert_eq!(
        out.text(),
        "[turn 1/10 - 0.9s] model replied: 2 tool calls (1.5k in, 80 out tokens)\n\
         [turn 1/10 - 2.0s] model replied: answer (1.5k in, 80 out tokens)\n\
         [turn 1/10 - 2.5s] model replied: answer (42 out tokens)\n"
    );
}

#[test]
fn failures_and_truncation_are_written_in_words() {
    let (mut progress, out) = plain();
    turn_started(&mut progress, 5, 0);
    progress.render(
        Some(5),
        "tool.failed",
        100,
        &json!({"call_id": "c", "tool": "fetch", "kind": "dispatch",
                "reason": "part 9 is out of range", "consecutive_failures": 1, "max_repairs": 3}),
    );
    progress.render(
        Some(5),
        "context.truncated",
        200,
        &json!({"dropped_turns": 2, "max_context_chars": 400_000}),
    );
    assert_eq!(
        out.text(),
        "[turn 5/10 - 0.1s] fetch call failed (dispatch): part 9 is out of range [failure 1 of 3 allowed]\n\
         [turn 5/10 - 0.2s] context truncated: dropped the oldest 2 turns\n"
    );
}

#[test]
fn plain_output_has_no_escape_codes_even_for_failures_and_the_last_line() {
    let (mut progress, out) = plain();
    turn_started(&mut progress, 1, 0);
    progress.render(
        Some(1),
        "tool.failed",
        1,
        &json!({"tool": "search", "kind": "execution", "reason": "boom",
                "consecutive_failures": 1, "max_repairs": 3}),
    );
    progress.render(
        None,
        "session.finished",
        2,
        &json!({"outcome": "tool_failure", "turns_used": 1, "turn_budget": 10,
                "usage": {}, "error": "boom"}),
    );
    assert!(!out.text().contains('\x1b'), "{:?}", out.text());
}

#[test]
fn auto_writes_nothing_without_a_terminal() {
    assert_eq!(ProgressMode::Auto.style(None, false, false), None);
}

#[test]
fn auto_on_a_terminal_colours_unless_no_color_is_set() {
    assert_eq!(
        ProgressMode::Auto.style(None, true, false),
        Some(Style::Tty { color: true })
    );
    assert_eq!(
        ProgressMode::Auto.style(None, true, true),
        Some(Style::Tty { color: false })
    );
}

#[test]
fn plain_forces_lines_when_piped_and_off_forces_nothing_on_a_terminal() {
    assert_eq!(
        ProgressMode::Plain.style(None, false, false),
        Some(Style::Plain)
    );
    assert_eq!(ProgressMode::Off.style(None, true, false), None);
}

#[test]
fn events_on_stderr_turn_progress_off_in_every_mode() {
    for mode in [ProgressMode::Auto, ProgressMode::Plain, ProgressMode::Off] {
        assert_eq!(mode.style(Some(&EventsSink::Stderr), true, false), None);
        assert!(!mode.draws_status_line(Some(&EventsSink::Stderr), true));
    }
    let file = EventsSink::File(PathBuf::from("out.ndjson"));
    assert_eq!(
        ProgressMode::Plain.style(Some(&file), false, false),
        Some(Style::Plain)
    );
}

#[test]
fn the_status_line_is_erased_before_every_permanent_line_and_at_the_end() {
    let (mut progress, out) = tty(false);
    turn_started(&mut progress, 1, 0);
    assert_eq!(out.text(), "turn 1/10: waiting for the model");
    progress.render(
        Some(1),
        "search.started",
        10,
        &json!({"call_id": "c", "queries": ["a", "b"], "recency": null}),
    );
    progress.render(
        Some(1),
        "search.finished",
        20,
        &json!({"ok": true, "queries": ["a", "b"], "hits": 1, "engines": [], "note": null}),
    );
    progress.render(
        None,
        "session.finished",
        30,
        &json!({"outcome": "ok", "turns_used": 1, "turn_budget": 10, "usage": {}}),
    );
    let text = out.text();
    // What a terminal shows: each row is what follows its last erase. Only
    // permanent lines may survive; no status text is left behind.
    let rows: Vec<&str> = text
        .split('\n')
        .map(|row| row.rsplit(CLEAR_LINE).next().unwrap_or(row))
        .collect();
    assert_eq!(rows.len(), 3, "{text:?}");
    assert!(rows[0].starts_with("[turn 1/10 - "), "{rows:?}");
    assert!(rows[1].starts_with("[0.0s] finished: ok"), "{rows:?}");
    assert_eq!(rows[2], "", "{rows:?}");
    assert!(text.contains("searching \"a\", \"b\"..."), "{text:?}");
}

#[test]
fn an_idle_clear_erases_only_a_status_that_is_showing() {
    let (mut progress, out) = tty(false);
    progress.clear();
    assert_eq!(out.text(), "");
    turn_started(&mut progress, 1, 0);
    progress.clear();
    progress.clear();
    assert_eq!(
        out.text(),
        format!("turn 1/10: waiting for the model{CLEAR_LINE}")
    );
}

#[test]
fn colour_never_replaces_the_words() {
    let (mut coloured, out) = tty(true);
    coloured.render(
        Some(1),
        "fetch.finished",
        1,
        &json!({"url": "https://x/", "ok": false, "error": "timeout"}),
    );
    let text = out.text();
    assert!(text.contains("\x1b[31m"), "{text:?}");
    let stripped: String = {
        let mut keep = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                keep.push(c);
            }
        }
        keep
    };
    assert!(
        stripped.contains("fetch https://x/ -> failed: timeout"),
        "{stripped:?}"
    );
}

struct Failing(Arc<std::sync::atomic::AtomicUsize>);

impl Write for Failing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(std::io::Error::other("closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_failed_write_disables_progress_without_ending_the_session() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut progress = Progress::new(Style::Plain, Box::new(Failing(Arc::clone(&attempts))));
    for turn in 1..=3 {
        turn_started(&mut progress, turn, 0);
        progress.render(
            Some(turn),
            "context.truncated",
            1,
            &json!({"dropped_turns": 1}),
        );
    }
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// The renderer reads what the emitter writes: a regression in an Event's
/// `data` shape shows here, not only in the NDJSON tests.
#[test]
fn the_emitter_feeds_progress_the_events_it_documents() {
    use crate::research::events::{Events, ToolFailure};

    let out = Shared::default();
    let events = Events::with_progress("s", Style::Plain, Box::new(out.clone()));
    events.turn_started(1, 6, false, None);
    events.tool_failed(
        1,
        &ToolFailure {
            call_id: None,
            tool: None,
            kind: "empty_answer",
            reason: "reply had no answer",
        },
        1,
        3,
    );
    events.context_truncated(1, 2, 1000);
    let text = out.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].starts_with("[turn 1/6 - ")
            && lines[0].ends_with(
                "empty reply failed (empty_answer): reply had no answer [failure 1 of 3 allowed]"
            ),
        "{text}"
    );
    assert!(
        lines[1].ends_with("context truncated: dropped the oldest 2 turns"),
        "{text}"
    );
}

mod stream;
