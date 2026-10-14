//! The text of Progress: what one Event says to a person. Pure functions of
//! the Event `data`, so the renderer stays about output and the wording is
//! testable without a terminal.

use serde_json::Value;

/// How a line is coloured on a terminal. The words of the line say the same
/// thing, so a reader without colour loses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Normal,
    Good,
    Bad,
    Warn,
}

/// An unsigned counter of `data`, `0` when absent.
pub(super) fn count_of(data: &Value, key: &str) -> u64 {
    data.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn text_of<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

/// `12345` as `12.3k`, `850` as `850`.
fn compact(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `text` cut to `max` characters with a trailing `...`, on one line.
pub(super) fn clip(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(3)).collect();
    cut.push_str("...");
    cut
}

/// The queries of a `search.started`, as `"q1", "q2"`.
fn queries(data: &Value) -> String {
    let list: Vec<String> = data
        .get("queries")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|query| format!("\"{}\"", clip(query, 60)))
                .collect()
        })
        .unwrap_or_default();
    if list.is_empty() {
        "search".to_owned()
    } else {
        format!("search {}", list.join(", "))
    }
}

/// `2m` for 100 s: suspensions are coarse, and the governor re-probes by the minute.
fn short_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs.div_ceil(60))
    }
}

/// `ddg 6, bing 5, yahoo 3; brave suspended 2m`: engines that answered, then
/// the ones that did not and why.
fn engines(list: &[Value]) -> String {
    let mut answered = Vec::new();
    let mut failed = Vec::new();
    for engine in list {
        let name = text_of(engine, "engine").unwrap_or("?");
        match text_of(engine, "status").unwrap_or("error") {
            "rows" | "no_results" => answered.push(format!("{name} {}", count_of(engine, "rows"))),
            "suspended" => {
                let secs = engine
                    .get("problems")
                    .and_then(Value::as_array)
                    .and_then(|problems| {
                        problems
                            .iter()
                            .find(|problem| text_of(problem, "kind") == Some("suspended"))
                    })
                    .map_or(0, |problem| count_of(problem, "remaining_secs"));
                failed.push(format!("{name} suspended {}", short_duration(secs)));
            }
            other => failed.push(format!("{name} {other}")),
        }
    }
    match (answered.is_empty(), failed.is_empty()) {
        (_, true) => answered.join(", "),
        (true, false) => failed.join(", "),
        (false, false) => format!("{}; {}", answered.join(", "), failed.join(", ")),
    }
}

/// `5 bullets: 4 cited, 3 exact, 1 partial, 0 none` from `answer.verified`.
pub(super) fn verification_counts(data: &Value) -> String {
    format!(
        "{}: {} cited, {} exact, {} partial, {} none",
        plural(count_of(data, "bullets"), "bullet", "bullets"),
        count_of(data, "cited"),
        count_of(data, "exact"),
        count_of(data, "partial"),
        count_of(data, "none"),
    )
}

/// What the step that `kind` starts is doing, for the transient status line.
/// `None` for Events that start no step.
pub(super) fn in_flight(kind: &str, data: &Value) -> Option<String> {
    match kind {
        "turn.started" => Some("waiting for the model".to_owned()),
        "search.started" => Some(format!(
            "{}...",
            queries(data).replacen("search", "searching", 1)
        )),
        "fetch.started" => {
            let url = text_of(data, "url").unwrap_or("?");
            Some(format!("fetching {url}..."))
        }
        _ => None,
    }
}

/// The permanent line of one Event, if it has one. `verification` is the
/// last `answer.verified`, repeated on the final line.
pub(super) fn describe(
    kind: &str,
    data: &Value,
    verification: Option<&str>,
) -> Option<(String, Tone)> {
    let line = match kind {
        "session.started" => (
            format!(
                "started: model {}, up to {}",
                text_of(data, "model").unwrap_or("?"),
                plural(count_of(data, "max_turns"), "turn", "turns"),
            ),
            Tone::Normal,
        ),
        "turn.started" if data.get("final_turn").and_then(Value::as_bool) == Some(true) => (
            format!(
                "final turn, no more tool calls ({})",
                text_of(data, "final_reason").unwrap_or("budget"),
            ),
            Tone::Warn,
        ),
        "model.replied" => (model_replied(data), Tone::Normal),
        "search.finished" => search_finished(data),
        "fetch.finished" => fetch_finished(data),
        "tool.failed" => (tool_failed(data), Tone::Bad),
        "context.truncated" => (
            format!(
                "context truncated: dropped the oldest {}",
                plural(count_of(data, "dropped_turns"), "turn", "turns"),
            ),
            Tone::Warn,
        ),
        "answer.verified" => (
            format!(
                "answer checked against the pages: {}",
                verification_counts(data)
            ),
            Tone::Normal,
        ),
        "session.finished" => session_finished(data, verification),
        _ => return None,
    };
    Some(line)
}

fn model_replied(data: &Value) -> String {
    let calls = data
        .get("tool_calls")
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    let usage = data.get("usage").unwrap_or(&Value::Null);
    let reply = if calls == 0 {
        "answer".to_owned()
    } else {
        plural(calls, "tool call", "tool calls")
    };
    // A gateway that does not report prompt tokens sends 0: say nothing
    // rather than print an input size that is not true.
    let tokens = match count_of(usage, "prompt_tokens") {
        0 => format!(
            "{} out tokens",
            compact(count_of(usage, "completion_tokens"))
        ),
        prompt => format!(
            "{} in, {} out tokens",
            compact(prompt),
            compact(count_of(usage, "completion_tokens")),
        ),
    };
    format!("model replied: {reply} ({tokens})")
}

fn search_finished(data: &Value) -> (String, Tone) {
    let label = queries(data);
    if data.get("ok").and_then(Value::as_bool) != Some(true) {
        let reason = text_of(data, "error").unwrap_or("unknown error");
        return (
            format!("{label} -> failed: {}", clip(reason, 160)),
            Tone::Bad,
        );
    }
    if let Some(note) = text_of(data, "note") {
        return (
            format!("{label} -> not searched: {}", clip(note, 160)),
            Tone::Warn,
        );
    }
    let hits = plural(count_of(data, "hits"), "hit", "hits");
    let list = data
        .get("engines")
        .and_then(Value::as_array)
        .map(|list| engines(list))
        .unwrap_or_default();
    let line = if list.is_empty() {
        format!("{label} -> {hits}")
    } else {
        format!("{label} -> {hits} ({list})")
    };
    (line, Tone::Normal)
}

fn fetch_finished(data: &Value) -> (String, Tone) {
    // The URL as requested: `final_url` is `null` on a failure.
    let url = text_of(data, "url").unwrap_or("?");
    if data.get("ok").and_then(Value::as_bool) != Some(true) {
        let reason = text_of(data, "error").unwrap_or("unknown error");
        return (
            format!("fetch {url} -> failed: {}", clip(reason, 160)),
            Tone::Bad,
        );
    }
    let served = text_of(data, "served_from");
    let parts = count_of(data, "parts");
    // A repeat without `part` delivers nothing: say so instead of `0 chars`.
    if count_of(data, "chars") == 0 && data.get("part").is_none_or(Value::is_null) {
        return (
            format!(
                "fetch {url} -> already fetched ({})",
                plural(parts, "part", "parts")
            ),
            Tone::Normal,
        );
    }
    let mut tags = Vec::new();
    match served {
        Some(from @ ("memory" | "tape")) => tags.push(from.to_owned()),
        _ => tags.extend(text_of(data, "fetch_path").map(str::to_owned)),
    }
    if let (Some(part), true) = (data.get("part").and_then(Value::as_u64), parts > 1) {
        tags.push(format!("part {part}/{parts}"));
    }
    let tags = if tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", tags.join(", "))
    };
    (
        format!(
            "fetch {url} -> {} chars{tags}",
            compact(count_of(data, "chars"))
        ),
        Tone::Normal,
    )
}

fn tool_failed(data: &Value) -> String {
    let what = match text_of(data, "tool") {
        Some(tool) => format!("{tool} call"),
        None => "empty reply".to_owned(),
    };
    format!(
        "{what} failed ({}): {} [failure {} of {} allowed]",
        text_of(data, "kind").unwrap_or("?"),
        clip(text_of(data, "reason").unwrap_or("no reason"), 160),
        count_of(data, "consecutive_failures"),
        count_of(data, "max_repairs"),
    )
}

fn session_finished(data: &Value, verification: Option<&str>) -> (String, Tone) {
    let outcome = text_of(data, "outcome").unwrap_or("?");
    let usage = data.get("usage").unwrap_or(&Value::Null);
    let mut line = format!(
        "finished: {outcome}, {}/{} turns, {} tokens",
        count_of(data, "turns_used"),
        count_of(data, "turn_budget"),
        compact(count_of(usage, "total_tokens")),
    );
    if outcome == "ok" {
        if let Some(counts) = verification {
            line.push_str(", ");
            line.push_str(counts);
        }
        return (line, Tone::Good);
    }
    // The reason is the text `main` prints after `error:`; a multi-line one
    // is flattened so the final line stays one line.
    if let Some(reason) = text_of(data, "error") {
        line.push_str(": ");
        line.push_str(&clip(reason, 300));
    }
    (line, Tone::Bad)
}
