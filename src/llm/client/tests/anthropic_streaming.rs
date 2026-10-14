//! Anthropic Messages SSE through the real `Gateway` HTTP path (#153):
//! canned `event:` + `data:` bodies as Anthropic sends them, the
//! `LlmEvent` order seen by the closure, replay blocks round-tripping into
//! the next request, retry of error and cut streams, and the `stream` wire
//! flag.

use std::time::Duration;

use super::super::*;
use super::helpers::{get_time_tools, messages, spawn_server, wire_body_of};
use crate::llm::request::ReplayBlocks;
use crate::test_support::CannedResponse;

/// Frame payloads like Anthropic does: `event: <type>` then `data: <json>`.
fn sse(payloads: &[&str]) -> String {
    payloads
        .iter()
        .map(|payload| {
            let kind = serde_json::from_str::<serde_json::Value>(payload)
                .ok()
                .and_then(|value| value["type"].as_str().map(str::to_owned))
                .expect("fixture payloads are JSON with a type");
            format!("event: {kind}\ndata: {payload}\n\n")
        })
        .collect()
}

fn event_stream(body: impl Into<String>) -> CannedResponse {
    CannedResponse::text(200, body).with_header("Content-Type", "text/event-stream")
}

fn gateway_with(base_url: String, attempts: u32, stream: bool) -> Gateway {
    let mut cfg = GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
        .with_max_attempts(attempts)
        .with_stream(stream);
    cfg.api_format = ApiFormat::Anthropic;
    Gateway::with_client(cfg, reqwest::Client::new())
}

fn gateway(base_url: String, attempts: u32) -> Gateway {
    gateway_with(base_url, attempts, true)
}

async fn observe(gateway: &Gateway) -> (Result<LlmReply, GatewayError>, Vec<LlmEvent>) {
    let mut events = Vec::new();
    let reply = gateway
        .chat_observed(&messages(), None, None, &mut |event| events.push(event))
        .await;
    (reply, events)
}

const START: &str = r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":3,"cache_creation_input_tokens":11,"cache_read_input_tokens":4096,"output_tokens":1}}}"#;

const TEXT_STREAM: [&str; 9] = [
    START,
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    r#"{"type":"ping"}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"gateway "}}"#,
    r#"{"type":"ping"}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
    r#"{"type":"content_block_stop","index":0}"#,
    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}"#,
    r#"{"type":"message_stop"}"#,
];

#[tokio::test]
async fn streamed_text_reply_reaches_the_closure_in_order_and_ignores_ping() {
    let (base, server) = spawn_server(vec![event_stream(sse(&TEXT_STREAM))]);
    let (reply, events) = observe(&gateway(base, 1)).await;
    let reply = reply.expect("stream must assemble");
    assert_eq!(reply.output, "gateway ok");
    assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        events,
        vec![
            LlmEvent::Text("gateway ".to_owned()),
            LlmEvent::Text("ok".to_owned())
        ]
    );
    assert_eq!(server.hits(), 1);
}

/// Cache read and creation come from `message_start`, `output_tokens`
/// from the closing `message_delta`.
#[tokio::test]
async fn usage_merges_message_start_and_message_delta_with_cache_counts() {
    let (base, _) = spawn_server(vec![event_stream(sse(&TEXT_STREAM))]);
    let usage = gateway(base, 1)
        .chat(&messages())
        .await
        .expect("stream must assemble")
        .usage;
    assert_eq!(usage.cached_prompt_tokens, 4096);
    assert_eq!(usage.cache_creation_prompt_tokens, 11);
    assert_eq!(usage.prompt_tokens, 3 + 11 + 4096);
    assert_eq!(usage.completion_tokens, 7);
    assert_eq!(usage.total_tokens, 3 + 11 + 4096 + 7);
}

/// Chunk boundaries fall mid-line and mid-JSON; two tool calls (after a
/// text block, so wire block indices are 1 and 2) keep their position in
/// the reply as the event `index`.
#[tokio::test]
async fn tool_use_json_split_across_network_chunks_assembles_by_call_position() {
    let body = sse(&[
        START,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking."}}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"search","input":{}}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"queries\": [\"ru"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"st\"]}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_b","name":"fetch","input":{}}}"#,
        r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"url\":\"https://e.x\"}"}}"#,
        r#"{"type":"content_block_stop","index":2}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let cut_at =
        |needle: &str, shift: usize| body.find(needle).expect("fixture has needle") + shift;
    let (first, rest) = body.split_at(cut_at("\"partial_json\":\"{\\\"queries", 12));
    let (second, third) =
        rest.split_at(rest.find("\nevent: content_block_start").expect("event") + 8);
    let (base, server) = spawn_server(vec![CannedResponse::text(200, "")
        .with_header("Content-Type", "text/event-stream")
        .with_chunks(vec![
            (first.to_owned(), Duration::ZERO),
            (second.to_owned(), Duration::from_millis(30)),
            (third.to_owned(), Duration::from_millis(30)),
        ])]);
    let (reply, events) = observe(&gateway(base, 1)).await;
    let reply = reply.expect("split stream assembles");
    assert_eq!(reply.output, "Checking.");
    assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(reply.tool_calls.len(), 2);
    assert_eq!(reply.tool_calls[0].id, "toolu_a");
    assert_eq!(reply.tool_calls[0].name, "search");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply.tool_calls[0].arguments)
            .expect("arguments are JSON"),
        serde_json::json!({"queries": ["rust"]})
    );
    assert_eq!(reply.tool_calls[1].id, "toolu_b");
    assert_eq!(reply.tool_calls[1].arguments, r#"{"url":"https://e.x"}"#);
    let indices: Vec<usize> = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ToolArgs { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(indices, vec![0, 0, 0, 1, 1], "{events:?}");
    assert!(
        matches!(
            events.last(),
            Some(LlmEvent::ToolArgs { index: 1, id: Some(id), name: Some(name), fragment })
                if id == "toolu_b" && name == "fetch" && fragment == r#"{"url":"https://e.x"}"#
        ),
        "{events:?}"
    );
    assert_eq!(server.hits(), 1);
}

const THINKING_STREAM: [&str; 13] = [
    START,
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The user wants "}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"the time."}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCkYIBRgC"}}"#,
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"IkDq/+9wxyz=="}}"#,
    r#"{"type":"content_block_stop","index":0}"#,
    r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"EmwKAhgBEgy3va3pzix/LafPsn4="}}"#,
    r#"{"type":"content_block_stop","index":1}"#,
    r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_t","name":"get_time","input":{}}}"#,
    r#"{"type":"content_block_stop","index":2}"#,
    r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}"#,
    r#"{"type":"message_stop"}"#,
];

/// Thinking arrives as `Thinking` events (the signature as none), and the
/// reply's replay blocks, fed into the next request, reach the wire with
/// `signature` and `data` intact and ahead of the `tool_use`.
#[tokio::test]
async fn thinking_and_redacted_blocks_round_trip_into_the_next_request() {
    let single = r#"{"type":"message","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
    let (base, server) = spawn_server(vec![
        event_stream(sse(&THINKING_STREAM)),
        CannedResponse::text(200, single),
    ]);
    let gateway = gateway(base, 1);
    let tools = get_time_tools();
    let mut events = Vec::new();
    let reply = gateway
        .chat_observed(&messages(), Some(tools.clone()), None, &mut |e| {
            events.push(e)
        })
        .await
        .expect("thinking stream assembles");

    assert_eq!(reply.thinking, "The user wants the time.");
    assert_eq!(
        events,
        vec![
            LlmEvent::Thinking("The user wants ".to_owned()),
            LlmEvent::Thinking("the time.".to_owned()),
            LlmEvent::ToolArgs {
                index: 0,
                id: Some("toolu_t".to_owned()),
                name: Some("get_time".to_owned()),
                fragment: String::new(),
            },
        ]
    );
    let expected = vec![
        serde_json::json!({
            "type": "thinking",
            "thinking": "The user wants the time.",
            "signature": "EqQBCkYIBRgCIkDq/+9wxyz==",
        }),
        serde_json::json!({
            "type": "redacted_thinking",
            "data": "EmwKAhgBEgy3va3pzix/LafPsn4=",
        }),
    ];
    assert_eq!(reply.replay.blocks, expected);

    let mut transcript = messages();
    transcript.push(ChatMessage::assistant_tool_calls(
        "",
        &reply.tool_calls,
        &reply.replay,
    ));
    transcript.push(ChatMessage::tool("toolu_t", "12:00"));
    gateway
        .chat_with_tools(&transcript, &tools, None)
        .await
        .expect("second turn");

    let body = wire_body_of(&server.requests()[1]);
    let assistant = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant turn is replayed");
    assert_eq!(assistant["content"][0], expected[0]);
    assert_eq!(assistant["content"][1], expected[1]);
    assert_eq!(assistant["content"][2]["type"], "tool_use");
    assert_eq!(
        assistant["content"][0]["signature"], "EqQBCkYIBRgCIkDq/+9wxyz==",
        "the signature is the concatenation of its deltas, untouched"
    );
}

#[tokio::test]
async fn retryable_error_event_mid_stream_restarts_the_attempt() {
    let broken = sse(&[
        START,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial "}}"#,
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    ]);
    let (base, server) = spawn_server(vec![event_stream(broken), event_stream(sse(&TEXT_STREAM))]);
    let (reply, events) = observe(&gateway(base, 2)).await;
    // Only the second attempt is assembled; the first one's "partial " was
    // streamed but belongs to a discarded attempt.
    assert_eq!(reply.expect("second attempt succeeds").output, "gateway ok");
    assert_eq!(server.hits(), 2);
    assert_eq!(events[0], LlmEvent::Text("partial ".to_owned()));
    assert!(
        matches!(
            &events[1],
            LlmEvent::Retry { attempt: 1, reason, .. } if reason.contains("overloaded_error")
        ),
        "{events:?}"
    );
    assert_eq!(
        &events[2..],
        &[
            LlmEvent::Text("gateway ".to_owned()),
            LlmEvent::Text("ok".to_owned())
        ]
    );
}

#[tokio::test]
async fn non_retryable_error_event_mid_stream_fails_at_once() {
    let body = sse(&[
        START,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}"#,
    ]);
    let (base, server) = spawn_server(vec![event_stream(body), event_stream(sse(&TEXT_STREAM))]);
    let (reply, events) = observe(&gateway(base, 3)).await;
    let err = reply.expect_err("must fail");
    assert!(
        matches!(&err, GatewayError::ErrorBody { retryable: false, message, .. } if message == "bad request"),
        "{err}"
    );
    assert_eq!(server.hits(), 1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::Retry { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn stream_cut_before_message_stop_is_retried_as_a_whole_attempt() {
    let cut = sse(&TEXT_STREAM[..5]);
    let (base, server) = spawn_server(vec![event_stream(cut), event_stream(sse(&TEXT_STREAM))]);
    let (reply, events) = observe(&gateway(base, 2)).await;
    assert_eq!(reply.expect("retry succeeds").output, "gateway ok");
    assert_eq!(server.hits(), 2);
    assert!(
        matches!(
            events.iter().find(|e| matches!(e, LlmEvent::Retry { .. })),
            Some(LlmEvent::Retry { attempt: 1, reason, .. }) if reason.contains("cut mid-body")
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn stream_cut_on_every_attempt_surfaces_a_retryable_transport_error() {
    let cut = sse(&TEXT_STREAM[..5]);
    let (base, server) = spawn_server(vec![event_stream(cut.clone()), event_stream(cut)]);
    let err = gateway(base, 2)
        .chat(&messages())
        .await
        .expect_err("both attempts are cut");
    assert!(matches!(err, GatewayError::Transport(_)), "{err}");
    assert!(err.is_retryable());
    assert_eq!(server.hits(), 2);
}

#[tokio::test]
async fn refusal_in_a_stream_is_not_retried() {
    let body = sse(&[
        START,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I can't help with that."}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":6}}"#,
        r#"{"type":"message_stop"}"#,
    ]);
    let (base, server) = spawn_server(vec![event_stream(body), event_stream(sse(&TEXT_STREAM))]);
    let err = gateway(base, 3)
        .chat(&messages())
        .await
        .expect_err("a refusal is an error");
    assert!(matches!(err, GatewayError::Refused(_)), "{err}");
    assert_eq!(server.hits(), 1);
}

/// The streamed reply is what the single-body path returns for the same
/// message, replay blocks and cache counts included, so flipping
/// `GATEWAY_STREAM` never changes what the loop sees.
#[tokio::test]
async fn streamed_reply_equals_the_single_body_reply() {
    let single = serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "The user wants the time.",
             "signature": "EqQBCkYIBRgCIkDq/+9wxyz=="},
            {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix/LafPsn4="},
            {"type": "tool_use", "id": "toolu_t", "name": "get_time", "input": {}}
        ],
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 3,
            "cache_creation_input_tokens": 11,
            "cache_read_input_tokens": 4096,
            "output_tokens": 30
        }
    })
    .to_string();
    let (base, _) = spawn_server(vec![event_stream(sse(&THINKING_STREAM))]);
    let streamed = gateway(base, 1).chat(&messages()).await.expect("sse");
    let (base, _) = spawn_server(vec![CannedResponse::text(200, single)]);
    let one_body = gateway_with(base, 1, false)
        .chat(&messages())
        .await
        .expect("json");
    assert_eq!(streamed.output, one_body.output);
    assert_eq!(streamed.thinking, one_body.thinking);
    assert_eq!(streamed.finish_reason, one_body.finish_reason);
    assert_eq!(streamed.tool_calls, one_body.tool_calls);
    assert_eq!(streamed.replay, one_body.replay);
    assert_ne!(streamed.replay, ReplayBlocks::default());
    assert_eq!(
        format!("{:?}", streamed.usage),
        format!("{:?}", one_body.usage)
    );
}

/// A gateway that ignores `stream: true` and answers `application/json`
/// keeps working, with no fragments for the closure.
#[tokio::test]
async fn json_content_type_falls_back_to_the_single_body_path() {
    let single = r#"{"type":"message","content":[{"type":"text","text":"gateway ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
    let (base, _) = spawn_server(vec![CannedResponse::text(200, single)]);
    let (reply, events) = observe(&gateway(base, 1)).await;
    assert_eq!(reply.expect("json body").output, "gateway ok");
    assert!(events.is_empty(), "{events:?}");
}

/// `GATEWAY_STREAM` drives the typed `stream` flag, and `GATEWAY_EXTRA_BODY`
/// cannot flip it either way.
#[tokio::test]
async fn stream_flag_follows_the_setting_and_beats_extra_body() {
    for (setting, extra, expected) in [(true, false, true), (false, true, false)] {
        let single = r#"{"type":"message","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let (base, server) = spawn_server(vec![CannedResponse::text(200, single)]);
        let mut cfg = GatewayConfig::new(base, "k".to_owned(), "m".to_owned())
            .with_max_attempts(1)
            .with_stream(setting);
        cfg.api_format = ApiFormat::Anthropic;
        cfg.extra_body = Some(serde_json::json!({"stream": extra}));
        Gateway::with_client(cfg, reqwest::Client::new())
            .chat(&messages())
            .await
            .expect("anthropic call must succeed");
        let body = wire_body_of(&server.requests()[0]);
        assert_eq!(body["stream"], serde_json::json!(expected), "{body}");
        assert!(body.get("stream_options").is_none(), "{body}");
    }
}
