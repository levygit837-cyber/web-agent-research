//! `model.delta` and `model.retry` through a whole hermetic Session (#154,
//! ADR-0010 §2 and §6): scripted SSE bodies in both wire formats, the
//! `--events-detail` filter on the NDJSON stream, the discard rule after a
//! mid-stream retry, and that the Session log keeps the retry but never a
//! delta.

use std::time::Duration;

use serde_json::Value;

use super::super::*;
use super::events::{cleanup, read_events, types};
use super::helpers::{hermetic_request, local_web, point_env_at, temp_session_out};
use crate::research::events::{EventsDetail, EventsSink};
use crate::research::session::read_session_rows;
use crate::test_support::{CannedResponse, CannedServer, EnvGuard};

const ENV: [&str; 4] = [
    "GATEWAY_BASE_URL",
    "GATEWAY_API_KEY",
    "GATEWAY_MODEL",
    "GATEWAY_API_FORMAT",
];

const ANSWER: &str = "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust.\n";
const SEARCH_ARGS: &str = r#"{"queries": ["obscura"]}"#;

#[derive(Clone, Copy, PartialEq)]
enum Wire {
    Openai,
    Anthropic,
}

fn stream(body: String) -> CannedResponse {
    CannedResponse::text(200, body).with_header("Content-Type", "text/event-stream")
}

/// OpenAI frames: `data: <json>`.
fn openai_frames(payloads: &[&str]) -> String {
    payloads
        .iter()
        .map(|payload| format!("data: {payload}\n\n"))
        .collect()
}

/// Anthropic frames: `event: <type>` then `data: <json>`.
fn anthropic_frames(payloads: &[Value]) -> String {
    payloads
        .iter()
        .map(|payload| {
            format!(
                "event: {}\ndata: {payload}\n\n",
                payload["type"].as_str().unwrap()
            )
        })
        .collect()
}

/// Turn 1: the model writes a `search` call in three fragments.
fn tool_turn(wire: Wire) -> CannedResponse {
    let (head, tail) = SEARCH_ARGS.split_at(14);
    let (mid, last) = tail.split_at(4);
    stream(match wire {
        Wire::Openai => {
            let call = |delta: Value| {
                serde_json::json!({"choices": [{"delta": {"tool_calls": [delta]}}]}).to_string()
            };
            let frames = [
                call(serde_json::json!({"index": 0, "id": "c1", "type": "function",
                    "function": {"name": "search", "arguments": head}})),
                call(serde_json::json!({"index": 0, "function": {"arguments": mid}})),
                call(serde_json::json!({"index": 0, "function": {"arguments": last}})),
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned(),
                r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#
                    .to_owned(),
                "[DONE]".to_owned(),
            ];
            openai_frames(&frames.iter().map(String::as_str).collect::<Vec<_>>())
        }
        Wire::Anthropic => anthropic_frames(&[
            serde_json::json!({"type": "message_start", "message": {"id": "m1", "type": "message",
                "role": "assistant", "content": [], "stop_reason": null,
                "usage": {"input_tokens": 1, "output_tokens": 1}}}),
            serde_json::json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {}}}),
            serde_json::json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": head}}),
            serde_json::json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": mid}}),
            serde_json::json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": last}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                "usage": {"output_tokens": 1}}),
            serde_json::json!({"type": "message_stop"}),
        ]),
    })
}

/// The answer, written in three text fragments.
fn answer_pieces() -> [&'static str; 3] {
    let (a, rest) = ANSWER.split_at(19);
    let (b, c) = rest.split_at(22);
    [a, b, c]
}

/// The final Turn: some thinking (Anthropic only), then the answer.
fn answer_turn(wire: Wire) -> CannedResponse {
    let pieces = answer_pieces();
    stream(match wire {
        Wire::Openai => {
            let text = |piece: &str| {
                serde_json::json!({"choices": [{"delta": {"content": piece}}]}).to_string()
            };
            let frames = [
                text(pieces[0]),
                text(pieces[1]),
                text(pieces[2]),
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_owned(),
                r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#
                    .to_owned(),
                "[DONE]".to_owned(),
            ];
            openai_frames(&frames.iter().map(String::as_str).collect::<Vec<_>>())
        }
        Wire::Anthropic => {
            let text = |piece: &str| {
                serde_json::json!({"type": "content_block_delta", "index": 1,
                    "delta": {"type": "text_delta", "text": piece}})
            };
            anthropic_frames(&[
                serde_json::json!({"type": "message_start", "message": {"id": "m2", "type": "message",
                    "role": "assistant", "content": [], "stop_reason": null,
                    "usage": {"input_tokens": 1, "output_tokens": 1}}}),
                serde_json::json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""}}),
                serde_json::json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "answer now"}}),
                serde_json::json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "signature_delta", "signature": "sig-1"}}),
                serde_json::json!({"type": "content_block_stop", "index": 0}),
                serde_json::json!({"type": "content_block_start", "index": 1,
                    "content_block": {"type": "text", "text": ""}}),
                text(pieces[0]),
                text(pieces[1]),
                text(pieces[2]),
                serde_json::json!({"type": "content_block_stop", "index": 1}),
                serde_json::json!({"type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"output_tokens": 1}}),
                serde_json::json!({"type": "message_stop"}),
            ])
        }
    })
}

/// A first attempt cut after a few words: no `finish_reason`, no `[DONE]`.
fn cut_attempt() -> CannedResponse {
    stream(openai_frames(&[
        r#"{"choices":[{"delta":{"content":"half an ans"}}]}"#,
    ]))
}

/// Run a Session over `responses` with `--events-detail detail`; return the
/// NDJSON Events and the Session log rows.
async fn run_session(
    tag: &str,
    wire: Wire,
    detail: EventsDetail,
    responses: Vec<CannedResponse>,
) -> (Vec<Value>, Vec<Value>) {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let server = CannedServer::spawn(responses);
    point_env_at(&format!("{}/v1", server.base()));
    if wire == Wire::Anthropic {
        std::env::set_var("GATEWAY_API_FORMAT", "anthropic");
    }
    let mut req = hermetic_request(tag);
    let events_path = temp_session_out(&format!("{tag}-events"));
    req.events = Some(EventsSink::File(events_path.clone()));
    req.events_detail = detail;
    let session_out = req.session_out.clone().expect("session out set");

    run_research_with(req, tools)
        .await
        .expect("session ends ok");

    let events = read_events(&events_path);
    let rows = std::fs::read_to_string(&session_out)
        .expect("Session file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("row"))
        .collect();
    assert!(
        read_session_rows(&session_out).is_ok(),
        "the file reads back"
    );
    cleanup(&[&events_path, &session_out]);
    (events, rows)
}

/// The types with runs of one type collapsed (`jq -r .type | uniq`).
fn collapsed(events: &[Value]) -> Vec<&str> {
    let mut kinds = types(events);
    kinds.dedup();
    kinds
}

/// The fragments of `channel` in Turn `turn`, from attempt `attempt`, joined.
fn joined(events: &[Value], turn: u64, attempt: u64, channel: &str) -> String {
    events
        .iter()
        .filter(|e| e["type"] == "model.delta" && e["turn"] == turn)
        .filter(|e| e["data"]["attempt"] == attempt && e["data"]["channel"] == channel)
        .map(|e| e["data"]["text"].as_str().expect("text").to_owned())
        .collect()
}

/// The `seq` of every Event row of the Session log.
fn log_seqs(rows: &[Value]) -> Vec<u64> {
    rows.iter()
        .filter(|row| row["kind"] == "event")
        .map(|row| row["seq"].as_u64().expect("seq"))
        .collect()
}

async fn deltas_precede_each_reply(wire: Wire, tag: &str) {
    let (events, rows) = run_session(
        tag,
        wire,
        EventsDetail::Tokens,
        vec![tool_turn(wire), answer_turn(wire)],
    )
    .await;

    // `read_events` already holds `seq` to 1, 2, 3, ... with no hole.
    assert_eq!(
        collapsed(&events),
        [
            "session.started",
            "turn.started",
            "model.delta",
            "model.replied",
            "search.started",
            "search.finished",
            "turn.started",
            "model.delta",
            "model.replied",
            "answer.verified",
            "session.finished",
        ]
    );
    // The call arrives in pieces that join into the arguments the loop ran:
    // one `index`, and the call identity once the model has said it.
    let args: Vec<&Value> = events
        .iter()
        .filter(|e| e["data"]["channel"] == "tool_args")
        .collect();
    assert!(args.len() >= 3, "{args:?}");
    assert!(args
        .iter()
        .all(|e| e["turn"] == 1 && e["data"]["index"] == 0));
    assert_eq!(joined(&events, 1, 1, "tool_args"), SEARCH_ARGS);
    assert!(args.iter().any(|e| e["data"]["name"] == "search"));
    let call_id = if wire == Wire::Openai {
        "c1"
    } else {
        "toolu_1"
    };
    assert!(args.iter().any(|e| e["data"]["id"] == call_id), "{args:?}");
    // The answer text is the reply's output, fragment by fragment.
    assert_eq!(joined(&events, 2, 1, "text"), ANSWER);
    if wire == Wire::Anthropic {
        assert_eq!(joined(&events, 2, 1, "thinking"), "answer now");
    }
    let replied = events
        .iter()
        .rfind(|e| e["type"] == "model.replied")
        .expect("a reply");
    assert_eq!(replied["data"]["output_chars"], ANSWER.chars().count());

    // The Session log never stores a delta, and shows the gaps they leave.
    assert!(rows.iter().all(|row| row["type"] != "model.delta"));
    let kept: Vec<u64> = events
        .iter()
        .filter(|e| e["type"] != "model.delta")
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(log_seqs(&rows), kept);
    assert!(kept.windows(2).any(|pair| pair[1] > pair[0] + 1));
}

#[tokio::test]
async fn tokens_detail_streams_deltas_before_each_reply_openai() {
    deltas_precede_each_reply(Wire::Openai, "stream-openai").await;
}

#[tokio::test]
async fn tokens_detail_streams_deltas_before_each_reply_anthropic() {
    deltas_precede_each_reply(Wire::Anthropic, "stream-anthropic").await;
}

/// Without `--events-detail tokens` the stream has no delta and no hole:
/// a Harness that sees a gap is seeing a real one.
#[tokio::test]
async fn normal_detail_leaves_deltas_out_without_a_seq_gap() {
    let (events, rows) = run_session(
        "stream-normal",
        Wire::Openai,
        EventsDetail::Normal,
        vec![tool_turn(Wire::Openai), answer_turn(Wire::Openai)],
    )
    .await;

    assert!(!types(&events).contains(&"model.delta"));
    // `read_events` held `seq` contiguous; the log is the same Events.
    assert_eq!(
        log_seqs(&rows),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
}

/// A cut first attempt: its deltas, then `model.retry`, then the second
/// attempt's deltas; the reply is the second attempt's alone.
#[tokio::test]
async fn a_retry_follows_the_failed_attempts_deltas_and_the_reply_matches_the_second() {
    let (events, rows) = run_session(
        "stream-retry",
        Wire::Openai,
        EventsDetail::Tokens,
        vec![cut_attempt(), answer_turn(Wire::Openai)],
    )
    .await;

    // A single Turn: the answer has no tool call.
    assert_eq!(
        collapsed(&events),
        [
            "session.started",
            "turn.started",
            "model.delta",
            "model.retry",
            "model.delta",
            "model.replied",
            "answer.verified",
            "session.finished",
        ]
    );
    let retry = events
        .iter()
        .find(|e| e["type"] == "model.retry")
        .expect("a retry");
    assert_eq!(retry["turn"], 1);
    assert_eq!(retry["data"]["attempt"], 1);
    assert!(retry["data"]["delay_ms"]
        .as_u64()
        .is_some_and(|ms| ms <= 1000));
    assert!(
        retry["data"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cut")),
        "{retry}"
    );
    assert_eq!(joined(&events, 1, 1, "text"), "half an ans");
    assert_eq!(joined(&events, 1, 2, "text"), ANSWER);
    let replied = events
        .iter()
        .find(|e| e["type"] == "model.replied")
        .unwrap();
    assert_eq!(replied["data"]["output_chars"], ANSWER.chars().count());

    // The log keeps the retry (it is an Event) and drops every delta.
    assert!(rows.iter().all(|row| row["type"] != "model.delta"));
    assert_eq!(
        rows.iter()
            .filter(|row| row["type"] == "model.retry")
            .count(),
        1
    );
}

/// `model.retry` is low volume: it is in the stream at the default detail,
/// where the failed attempt's deltas never were.
#[tokio::test]
async fn model_retry_is_emitted_at_the_default_detail() {
    let (events, _) = run_session(
        "stream-retry-normal",
        Wire::Openai,
        EventsDetail::Normal,
        vec![cut_attempt(), answer_turn(Wire::Openai)],
    )
    .await;

    assert_eq!(
        collapsed(&events),
        [
            "session.started",
            "turn.started",
            "model.retry",
            "model.replied",
            "answer.verified",
            "session.finished",
        ]
    );
    // Kept short: the retry sleeps at most the first backoff ceiling.
    assert!(
        Duration::from_millis(events[2]["data"]["delay_ms"].as_u64().unwrap())
            <= Duration::from_secs(1)
    );
}
