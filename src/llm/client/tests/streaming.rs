//! OpenAI SSE through the real `Gateway` HTTP path (#152): canned
//! `text/event-stream` bodies, the `LlmEvent` order seen by the closure,
//! retry of cut and error streams, the idle bound, and the `Content-Type`
//! fallback to the single-body path.

use std::time::Duration;

use super::super::*;
use super::helpers::{messages, ok, spawn_server, SUCCESS_BODY};
use crate::test_support::CannedResponse;

fn sse(payloads: &[&str]) -> String {
    payloads
        .iter()
        .map(|payload| format!("data: {payload}\n\n"))
        .collect()
}

fn event_stream(body: impl Into<String>) -> CannedResponse {
    CannedResponse::text(200, body).with_header("Content-Type", "text/event-stream")
}

fn gateway(base_url: String, attempts: u32) -> Gateway {
    Gateway::with_client(
        GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
            .with_max_attempts(attempts),
        reqwest::Client::new(),
    )
}

async fn observe(gateway: &Gateway) -> (Result<LlmReply, GatewayError>, Vec<LlmEvent>) {
    let mut events = Vec::new();
    let reply = gateway
        .chat_observed(&messages(), None, None, &mut |event| events.push(event))
        .await;
    (reply, events)
}

const TEXT_STREAM: [&str; 5] = [
    r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
    r#"{"choices":[{"delta":{"content":"gateway "}}]}"#,
    r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
    r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":null}"#,
    "[DONE]",
];

#[tokio::test]
async fn streamed_text_reply_reaches_the_closure_in_order_and_assembles() {
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

/// The assembled reply is what the single-body path returns for the same
/// content, so flipping `GATEWAY_STREAM` never changes what the loop sees.
#[tokio::test]
async fn streamed_reply_equals_the_single_body_reply() {
    let streamed = sse(&[
        r#"{"choices":[{"delta":{"content":"gateway ok"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        "[DONE]",
    ]);
    let (base, _) = spawn_server(vec![event_stream(streamed)]);
    let streamed = gateway(base, 1).chat(&messages()).await.expect("sse");
    let (base, _) = spawn_server(vec![ok()]);
    let single = gateway(base, 1).chat(&messages()).await.expect("json");
    assert_eq!(streamed.output, single.output);
    assert_eq!(streamed.finish_reason, single.finish_reason);
    assert_eq!(streamed.thinking, single.thinking);
    assert_eq!(
        format!("{:?}", streamed.usage),
        format!("{:?}", single.usage)
    );
}

/// Chunk boundaries fall mid-event and mid-JSON; the interleaved tool
/// calls still assemble by `index`.
#[tokio::test]
async fn tool_calls_split_across_network_chunks_assemble_by_index() {
    let body = sse(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"search","arguments":""}},{"index":1,"id":"call_b","type":"function","function":{"name":"fetch","arguments":""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"url\":\"https://x\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"queries\":[\"q\"]}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    let pieces: Vec<(String, Duration)> = body
        .as_bytes()
        .chunks(37)
        .map(|piece| (String::from_utf8_lossy(piece).into_owned(), Duration::ZERO))
        .collect();
    // 37-byte cuts may split a multi-byte char; the body is ASCII, so none do.
    assert!(body.is_ascii());
    let (base, _) = spawn_server(vec![CannedResponse::text(200, "")
        .with_header("Content-Type", "text/event-stream")
        .with_chunks(pieces)]);
    let (reply, events) = observe(&gateway(base, 1)).await;
    let reply = reply.expect("split stream must assemble");
    assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
    let calls: Vec<_> = reply
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.as_str()))
        .collect();
    assert_eq!(
        calls,
        vec![
            ("call_a", "search", r#"{"queries":["q"]}"#),
            ("call_b", "fetch", r#"{"url":"https://x"}"#),
        ]
    );
    let fragments_of = |wanted: usize| -> String {
        events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::ToolArgs {
                    index, fragment, ..
                } if *index == wanted => Some(fragment.as_str()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(fragments_of(0), r#"{"queries":["q"]}"#);
    assert_eq!(fragments_of(1), r#"{"url":"https://x"}"#);
}

#[tokio::test]
async fn reasoning_streams_as_thinking_before_the_answer_text() {
    let body = sse(&[
        r#"{"choices":[{"delta":{"reasoning_content":"plan "}}]}"#,
        r#"{"choices":[{"delta":{"reasoning_content":"first"}}]}"#,
        r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);
    let (base, _) = spawn_server(vec![event_stream(body)]);
    let (reply, events) = observe(&gateway(base, 1)).await;
    let reply = reply.expect("assembles");
    assert_eq!(reply.thinking, "plan first");
    assert_eq!(reply.output, "done");
    assert_eq!(
        events,
        vec![
            LlmEvent::Thinking("plan ".to_owned()),
            LlmEvent::Thinking("first".to_owned()),
            LlmEvent::Text("done".to_owned()),
        ]
    );
}

#[tokio::test]
async fn usage_arrives_on_the_final_chunk_after_finish_reason() {
    let body = sse(&[
        r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":8},"completion_tokens_details":{"reasoning_tokens":2}}}"#,
        "[DONE]",
    ]);
    let (base, _) = spawn_server(vec![event_stream(body)]);
    let usage = gateway(base, 1)
        .chat(&messages())
        .await
        .expect("assembles")
        .usage;
    assert_eq!(
        (
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens,
            usage.cached_prompt_tokens,
            usage.reasoning_tokens
        ),
        (11, 3, 14, 8, 2)
    );
}

#[tokio::test]
async fn refusal_in_a_stream_is_not_retried() {
    let body = sse(&[
        r#"{"choices":[{"delta":{"refusal":"no can do"},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);
    let (base, server) = spawn_server(vec![event_stream(body), event_stream("unused")]);
    let err = gateway(base, 3)
        .chat(&messages())
        .await
        .expect_err("refusal is an error");
    assert!(matches!(err, GatewayError::Refused(text) if text == "no can do"));
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn retryable_error_object_mid_stream_restarts_the_attempt() {
    let broken = sse(&[
        r#"{"choices":[{"delta":{"content":"partial "}}]}"#,
        r#"{"error":{"type":"overloaded_error","message":"busy"}}"#,
    ]);
    let (base, server) = spawn_server(vec![event_stream(broken), event_stream(sse(&TEXT_STREAM))]);
    let (reply, events) = observe(&gateway(base, 2)).await;
    // The reply holds only the second attempt's content: the failed
    // attempt's "partial " was streamed but never assembled.
    assert_eq!(reply.expect("second attempt succeeds").output, "gateway ok");
    assert_eq!(server.hits(), 2);
    let retry = events
        .iter()
        .position(|event| matches!(event, LlmEvent::Retry { .. }))
        .expect("a Retry event separates the attempts");
    assert_eq!(events[0], LlmEvent::Text("partial ".to_owned()));
    assert_eq!(
        retry, 1,
        "fragments of attempt 1 precede the Retry: {events:?}"
    );
    assert!(
        matches!(
            &events[retry],
            LlmEvent::Retry { attempt: 1, reason, .. } if reason.contains("overloaded_error")
        ),
        "{events:?}"
    );
    assert_eq!(
        &events[retry + 1..],
        &[
            LlmEvent::Text("gateway ".to_owned()),
            LlmEvent::Text("ok".to_owned())
        ]
    );
}

#[tokio::test]
async fn non_retryable_error_object_mid_stream_fails_at_once() {
    let body = sse(&[
        r#"{"choices":[{"delta":{"content":"par"}}]}"#,
        r#"{"error":{"code":"invalid_request_error","message":"bad request"}}"#,
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
async fn stream_cut_before_finish_is_retried_as_a_whole_attempt() {
    let cut = sse(&[r#"{"choices":[{"delta":{"content":"half an ans"}}]}"#]);
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
    let cut = sse(&[r#"{"choices":[{"delta":{"content":"half"}}]}"#]);
    let (base, server) = spawn_server(vec![event_stream(cut.clone()), event_stream(cut)]);
    let err = gateway(base, 2)
        .chat(&messages())
        .await
        .expect_err("both attempts are cut");
    assert!(matches!(err, GatewayError::Transport(_)), "{err}");
    assert!(err.is_retryable());
    assert_eq!(server.hits(), 2);
}

/// A finish_reason without `[DONE]` is a complete reply: some gateways
/// close the connection right after the last chunk.
#[tokio::test]
async fn stream_with_finish_reason_but_no_done_is_complete() {
    let body = sse(&[r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#]);
    let (base, _) = spawn_server(vec![event_stream(body)]);
    let reply = gateway(base, 1).chat(&messages()).await.expect("complete");
    assert_eq!(reply.output, "ok");
}

#[tokio::test]
async fn silence_longer_than_the_idle_bound_is_a_timeout() {
    let first = sse(&[r#"{"choices":[{"delta":{"content":"hi"}}]}"#]);
    let (base, _) = spawn_server(vec![CannedResponse::text(200, "")
        .with_header("Content-Type", "text/event-stream")
        .with_chunks(vec![
            (first, Duration::ZERO),
            ("data: [DONE]\n\n".to_owned(), Duration::from_millis(1500)),
        ])]);
    let config = GatewayConfig::new(base, "test-key".to_owned(), "m".to_owned())
        .with_max_attempts(1)
        .with_stream_idle_timeout(Duration::from_millis(200));
    let gateway = Gateway::with_client(config, reqwest::Client::new());
    let started = std::time::Instant::now();
    let (reply, events) = observe(&gateway).await;
    assert!(matches!(reply, Err(GatewayError::Timeout)), "{reply:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "the idle bound, not the stalled server, ends the wait"
    );
    assert_eq!(events, vec![LlmEvent::Text("hi".to_owned())]);
}

/// Steady chunks within the idle bound are fine however long the whole
/// body takes: the bound is per gap, not per body.
#[tokio::test]
async fn slow_but_steady_stream_is_not_an_idle_timeout() {
    let pieces: Vec<(String, Duration)> = TEXT_STREAM
        .iter()
        .map(|payload| (format!("data: {payload}\n\n"), Duration::from_millis(120)))
        .collect();
    let (base, _) = spawn_server(vec![CannedResponse::text(200, "")
        .with_header("Content-Type", "text/event-stream")
        .with_chunks(pieces)]);
    let config = GatewayConfig::new(base, "test-key".to_owned(), "m".to_owned())
        .with_max_attempts(1)
        .with_stream_idle_timeout(Duration::from_millis(400));
    let reply = Gateway::with_client(config, reqwest::Client::new())
        .chat(&messages())
        .await
        .expect("each gap is shorter than the idle bound");
    assert_eq!(reply.output, "gateway ok");
}

/// A gateway that ignores `stream: true` and answers with one JSON body
/// keeps working, and the closure sees no fragments.
#[tokio::test]
async fn json_content_type_falls_back_to_the_single_body_path() {
    let (base, server) = spawn_server(vec![CannedResponse::text(200, SUCCESS_BODY)]);
    let (reply, events) = observe(&gateway(base, 1)).await;
    assert_eq!(reply.expect("json path").output, "gateway ok");
    assert!(events.is_empty(), "{events:?}");
    let body = super::helpers::wire_body_of(&server.requests()[0]);
    assert_eq!(body["stream"], serde_json::json!(true));
}

#[tokio::test]
async fn sse_body_is_read_even_when_streaming_is_off() {
    // The response, not the flag, picks the reader: a gateway that streams
    // regardless must not hit the JSON parser.
    let (base, _) = spawn_server(vec![event_stream(sse(&TEXT_STREAM))]);
    let config = GatewayConfig::new(base, "test-key".to_owned(), "m".to_owned())
        .with_max_attempts(1)
        .with_stream(false);
    let reply = Gateway::with_client(config, reqwest::Client::new())
        .chat(&messages())
        .await
        .expect("sse body parses");
    assert_eq!(reply.output, "gateway ok");
}

/// `Retry` fires for non-stream failures too (a 503 before the first byte).
#[tokio::test]
async fn retry_event_precedes_the_backoff_for_status_failures() {
    let (base, _) = spawn_server(vec![CannedResponse::text(503, "down"), ok()]);
    let (reply, events) = observe(&gateway(base, 2)).await;
    reply.expect("second attempt succeeds");
    assert!(
        matches!(
            events.as_slice(),
            [LlmEvent::Retry { attempt: 1, delay_ms, reason }]
                if *delay_ms <= 1000 && reason.contains("HTTP 503")
        ),
        "{events:?}"
    );
}
