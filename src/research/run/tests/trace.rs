//! `--trace` (#151): the Session file holds what the model saw and said,
//! and nothing that could authenticate to the gateway.

use serde_json::Value;

use super::super::*;
use super::helpers::{
    hermetic_request, local_web, point_env_at, session_rows, spawn_capturing_server, spawn_server,
    temp_session_out, text_body, tool_call, tools_body, wire_body_of,
};
use crate::test_support::{CannedResponse, CannedServer, EnvGuard};

const ENV: [&str; 3] = ["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"];

fn search_call() -> String {
    tools_body(
        vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
        "",
    )
}

const ANSWER: &str = "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust.\n";

fn trace_rows(rows: &[Value]) -> Vec<&Value> {
    rows.iter().filter(|row| row["kind"] == "trace").collect()
}

/// Rebuild the request of every `model` row from the file alone, following
/// the documented rule: the first row gives `system`, a `full` row replaces
/// the transcript, any other appends its delta.
fn rebuild_requests(rows: &[Value]) -> Vec<Vec<Value>> {
    let mut system = Value::Null;
    let mut transcript: Vec<Value> = Vec::new();
    let mut requests = Vec::new();
    for row in trace_rows(rows)
        .into_iter()
        .filter(|r| r["phase"] == "model")
    {
        if !row["system"].is_null() {
            system = serde_json::json!({ "role": "system", "content": row["system"] });
        }
        if row["full"] == true {
            transcript = row["messages"].as_array().expect("full row").clone();
        } else {
            transcript.extend(row["messages_delta"].as_array().expect("delta").clone());
        }
        let mut request = vec![system.clone()];
        request.extend(transcript.clone());
        requests.push(request);
    }
    requests
}

/// A traced Session with one tool Turn and one answer Turn: the rows alone
/// give back, Turn by Turn, the exact messages the gateway received, and the
/// tool results row holds the content the model got.
#[tokio::test]
async fn a_traced_session_rebuilds_the_exact_messages_sent_each_turn() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let (base, server) = spawn_capturing_server(vec![search_call(), text_body(ANSWER)]);
    point_env_at(&base);
    let mut req = hermetic_request("trace-ok");
    req.trace = true;
    let session_out = req.session_out.clone().expect("session out set");

    run_research_with(req, tools).await.expect("answers");

    let rows = session_rows(&std::fs::read_to_string(&session_out).unwrap());
    let wire: Vec<Vec<Value>> = server
        .requests()
        .iter()
        .map(|raw| wire_body_of(raw)["messages"].as_array().unwrap().clone())
        .collect();
    assert_eq!(wire.len(), 2);
    assert_eq!(rebuild_requests(&rows), wire, "rows rebuild what was sent");

    let traces = trace_rows(&rows);
    let shape: Vec<(u64, &str)> = traces
        .iter()
        .map(|r| (r["turn"].as_u64().unwrap(), r["phase"].as_str().unwrap()))
        .collect();
    assert_eq!(shape, [(1, "model"), (1, "tool_results"), (2, "model")]);
    for row in &traces {
        assert_eq!(row["format_version"], 2);
        assert_eq!(row["session_id"], "test-trace-ok");
    }

    // System prompt and tools once, on Turn 1; later rows are deltas.
    assert!(traces[0]["system"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(traces[0]["tools"].as_array().is_some_and(|t| t.len() == 2));
    assert_eq!(traces[2]["system"], Value::Null);
    assert_eq!(traces[2]["tools"], Value::Null);
    assert_eq!(traces[0]["tool_choice"], "auto");
    assert_eq!(traces[2]["messages_delta"].as_array().unwrap().len(), 2);

    // The reply as received, tool call arguments raw.
    let call = &traces[0]["reply"]["tool_calls"][0];
    assert_eq!(call["id"], "c1");
    assert_eq!(call["name"], "search");
    assert_eq!(call["arguments"], r#"{"queries": ["obscura"]}"#);
    assert_eq!(traces[2]["reply"]["output"], ANSWER);
    assert!(traces[2]["reply"]["usage"].is_object());

    // Tool results: exactly the `tool` message of Turn 2's request.
    let results = traces[1]["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"], "c1");
    let sent_tool = &wire[1][3];
    assert_eq!(sent_tool["role"], "tool");
    assert_eq!(results[0]["content"], sent_tool["content"]);
    assert!(results[0]["content"].as_str().unwrap().contains("<hit"));

    let _ = std::fs::remove_file(&session_out);
}

/// Without `--trace` the Session file has no `trace` row at all.
#[tokio::test]
async fn an_untraced_session_writes_no_trace_rows() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    point_env_at(&spawn_server(vec![search_call(), text_body(ANSWER)]));
    let req = hermetic_request("trace-off");
    let session_out = req.session_out.clone().expect("session out set");

    run_research_with(req, tools).await.expect("answers");

    let rows = session_rows(&std::fs::read_to_string(&session_out).unwrap());
    assert!(trace_rows(&rows).is_empty(), "{rows:?}");
    let _ = std::fs::remove_file(&session_out);
}

/// A failed gateway call is part of the audit trail, and the file still
/// closes with `session.finished`. `--events` never sees a trace.
#[tokio::test]
async fn a_gateway_failure_turn_is_traced_with_its_error_and_the_file_closes() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let server = CannedServer::spawn(vec![
        CannedResponse::text(200, search_call()),
        CannedResponse::text(401, "{}"),
    ]);
    point_env_at(&format!("{}/v1", server.base()));
    let mut req = hermetic_request("trace-fail");
    req.trace = true;
    let events_path = temp_session_out("trace-fail-events");
    req.events = Some(crate::research::events::EventsSink::File(
        events_path.clone(),
    ));
    let session_out = req.session_out.clone().expect("session out set");

    let err = run_research_with(req, tools)
        .await
        .expect_err("401 is final");
    assert!(matches!(err, ResearchError::GatewayRejected(_)), "{err:?}");

    let rows = session_rows(&std::fs::read_to_string(&session_out).unwrap());
    let traces = trace_rows(&rows);
    let shape: Vec<(u64, &str)> = traces
        .iter()
        .map(|r| (r["turn"].as_u64().unwrap(), r["phase"].as_str().unwrap()))
        .collect();
    assert_eq!(shape, [(1, "model"), (1, "tool_results"), (2, "model")]);
    let failed = traces[2];
    assert!(failed["reply"]["error"]
        .as_str()
        .is_some_and(|e| e.contains("auth")));
    assert_eq!(failed["reply"]["output"], Value::Null);
    assert_eq!(rows.last().unwrap()["type"], "session.finished");

    let events = std::fs::read_to_string(&events_path).unwrap();
    assert!(!events.contains("\"trace\""), "{events}");
    let _ = std::fs::remove_file(&events_path);
    let _ = std::fs::remove_file(&session_out);
}

/// The API key goes out on the wire and nowhere into the file.
#[tokio::test]
async fn the_api_key_never_reaches_the_session_file() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let (base, server) = spawn_capturing_server(vec![search_call(), text_body(ANSWER)]);
    point_env_at(&base);
    let key = "sk-trace-secret-7f3a9c1e";
    std::env::set_var("GATEWAY_API_KEY", key);
    let mut req = hermetic_request("trace-secret");
    req.trace = true;
    let session_out = req.session_out.clone().expect("session out set");

    run_research_with(req, tools).await.expect("answers");

    assert!(
        server.requests().iter().all(|raw| raw.contains(key)),
        "the key was sent, so the absence below proves something"
    );
    let file = std::fs::read_to_string(&session_out).unwrap();
    assert!(file.contains("\"kind\":\"trace\""));
    assert!(!file.contains(key), "the Session file leaks the API key");
    let _ = std::fs::remove_file(&session_out);
}
