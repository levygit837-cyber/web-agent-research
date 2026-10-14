//! Lifecycle Events of a hermetic Session (#147, ADR-0010): the sequence and
//! fields of `session.started` → `turn.started` → `model.replied` → … →
//! `session.finished`, and that `session.finished` closes every exit.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::super::*;
use super::helpers::{
    hermetic_request, local_web, point_env_at, spawn_capturing_server, spawn_server,
    temp_session_out, text_body, tool_call, tools_body, walled_web,
};
use crate::research::events::EventsSink;
use crate::test_support::{CannedResponse, CannedServer, EnvGuard};

const ENV: [&str; 3] = ["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"];

fn request_with_events(tag: &str) -> (ResearchRequest, std::path::PathBuf, std::path::PathBuf) {
    let mut req = hermetic_request(tag);
    let events_path = temp_session_out(&format!("{tag}-events"));
    req.events = Some(EventsSink::File(events_path.clone()));
    let session_out = req.session_out.clone().expect("session out set");
    (req, events_path, session_out)
}

/// Read the NDJSON file back and hold it to the envelope contract: every
/// line is one envelope of this Session, `seq` is 1, 2, 3, … and
/// `session.finished` appears once, last.
fn read_events(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("events file exists");
    let events: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON envelope"))
        .collect();
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["v"], 1, "{event}");
        assert_eq!(
            event["seq"],
            index as u64 + 1,
            "seq strictly increases: {event}"
        );
        assert!(event["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));
        let ts = event["ts"].as_str().expect("ts is a string");
        assert!(
            ts.ends_with('Z') && ts.len() == 24,
            "RFC 3339 with ms: {ts}"
        );
        assert!(event["elapsed_ms"].is_u64(), "{event}");
        assert!(event["data"].is_object(), "{event}");
    }
    let finished = events
        .iter()
        .filter(|event| event["type"] == "session.finished")
        .count();
    assert_eq!(finished, 1, "exactly one session.finished");
    assert_eq!(events.last().expect("events")["type"], "session.finished");
    events
}

fn types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["type"].as_str().expect("type"))
        .collect()
}

fn finished(events: &[Value]) -> &Value {
    &events.last().expect("events")["data"]
}

fn cleanup(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

#[tokio::test]
async fn a_successful_session_emits_its_lifecycle_in_order() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, page_url) = local_web().await;
    let answer = format!(
        "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust per [Obscura]({page_url}).\n"
    );
    let fetch_args = serde_json::json!({ "url": page_url }).to_string();
    point_env_at(&spawn_server(vec![
        tools_body(
            vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
            "",
        ),
        tools_body(vec![tool_call("c2", "fetch", &fetch_args)], "fetching now"),
        text_body(&answer),
    ]));
    let (req, events_path, session_out) = request_with_events("events-ok");

    let response = run_research_with(req, tools)
        .await
        .expect("session succeeds");

    let events = read_events(&events_path);
    assert_eq!(
        types(&events),
        [
            "session.started",
            "turn.started",
            "model.replied",
            "turn.started",
            "model.replied",
            "turn.started",
            "model.replied",
            "session.finished",
        ]
    );
    assert!(events
        .iter()
        .all(|event| event["session_id"] == "test-events-ok"));

    let started = &events[0];
    assert!(
        started.get("turn").is_none(),
        "Session-level Events carry no turn"
    );
    assert_eq!(
        started["data"]["goal"],
        "What is the Obscura headless browser?"
    );
    assert_eq!(started["data"]["size"], "small");
    assert_eq!(started["data"]["max_turns"], 8);
    assert_eq!(started["data"]["max_turns_cap"], 25);
    assert_eq!(started["data"]["deadline_secs"], 300);
    assert_eq!(started["data"]["api_format"], "openai");
    assert!(started["data"]["model"]
        .as_str()
        .is_some_and(|m| !m.is_empty()));
    assert!(started["data"]["stream"].is_boolean());

    let turn_started = [&events[1], &events[3], &events[5]];
    for (index, event) in turn_started.iter().enumerate() {
        assert_eq!(event["turn"], index as u64 + 1);
        assert_eq!(event["data"]["turn_budget"], 8);
        assert_eq!(event["data"]["turns_left"], 7 - index as u64);
        assert_eq!(event["data"]["final_turn"], false);
        assert_eq!(event["data"]["final_reason"], Value::Null);
    }

    let first_reply = &events[2];
    assert_eq!(first_reply["turn"], 1);
    assert_eq!(first_reply["data"]["finish_reason"], "tool_calls");
    assert_eq!(
        first_reply["data"]["tool_calls"],
        serde_json::json!([{ "id": "c1", "name": "search" }])
    );
    assert_eq!(first_reply["data"]["usage"]["total_tokens"], 2);
    let last_reply = &events[6];
    assert_eq!(last_reply["data"]["finish_reason"], "stop");
    assert_eq!(last_reply["data"]["tool_calls"], serde_json::json!([]));
    assert_eq!(
        last_reply["data"]["output_chars"],
        answer.chars().count() as u64
    );

    let end = finished(&events);
    assert_eq!(end["outcome"], "ok");
    assert_eq!(end["exit_code"], 0);
    assert_eq!(end["turns_used"], response.turns_used);
    assert_eq!(end["turn_budget"], response.turn_budget);
    assert_eq!(end["usage"]["total_tokens"], 6);
    assert_eq!(end["evidence_urls"], serde_json::json!([page_url]));
    assert_eq!(end["error"], Value::Null);
    cleanup(&[&events_path, &session_out]);
}

/// A one-Turn budget makes Turn 1 the final Turn, and its reason is the budget.
#[tokio::test]
async fn budget_exhaustion_ends_with_exit_5_after_a_final_turn() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    point_env_at(&spawn_server(vec![tools_body(
        vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
        "",
    )]));
    let (mut req, events_path, session_out) = request_with_events("events-budget");
    req.max_turns = 1;
    req.max_turns_cap = 1;

    let err = run_research_with(req, tools).await.expect_err("no answer");
    assert!(
        matches!(err, ResearchError::BudgetExhausted { turns: 1 }),
        "{err:?}"
    );

    let events = read_events(&events_path);
    assert_eq!(
        types(&events),
        [
            "session.started",
            "turn.started",
            "model.replied",
            "session.finished"
        ]
    );
    assert_eq!(events[1]["data"]["final_turn"], true);
    assert_eq!(events[1]["data"]["final_reason"], "budget");
    assert_eq!(events[1]["data"]["turns_left"], 0);
    let end = finished(&events);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("budget_exhausted"), Some(5))
    );
    assert_eq!(end["turns_used"], 1);
    assert_eq!(end["turn_budget"], 1);
    assert_eq!(
        end["usage"]["total_tokens"], 2,
        "a failed Session still reports what it spent"
    );
    assert!(end["error"].as_str().is_some_and(|e| e.contains("budget")));
    cleanup(&[&events_path, &session_out]);
}

#[tokio::test]
async fn a_rejecting_gateway_ends_with_exit_8_and_no_reply() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let server = CannedServer::spawn(vec![CannedResponse::text(401, "{}")]);
    point_env_at(&format!("{}/v1", server.base()));
    let (req, events_path, session_out) = request_with_events("events-rejected");

    let err = run_research_with(req, tools)
        .await
        .expect_err("401 is final");
    assert!(matches!(err, ResearchError::GatewayRejected(_)), "{err:?}");

    let events = read_events(&events_path);
    assert_eq!(
        types(&events),
        ["session.started", "turn.started", "session.finished"]
    );
    let end = finished(&events);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("gateway_rejected"), Some(8))
    );
    assert_eq!(end["turns_used"], 1);
    cleanup(&[&events_path, &session_out]);
}

#[tokio::test]
async fn a_deadline_with_no_answer_ends_with_exit_9() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let server = CannedServer::spawn(vec![
        CannedResponse::text(200, text_body("too late")).with_delay(Duration::from_secs(3))
    ]);
    point_env_at(&format!("{}/v1", server.base()));
    let (mut req, events_path, session_out) = request_with_events("events-deadline");
    req.deadline_secs = 1;

    let err = run_research_with(req, tools)
        .await
        .expect_err("deadline passes");
    assert!(
        matches!(err, ResearchError::DeadlineExceeded { .. }),
        "{err:?}"
    );

    let events = read_events(&events_path);
    assert_eq!(events[0]["data"]["deadline_secs"], 1);
    let end = finished(&events);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("deadline_exceeded"), Some(9))
    );
    cleanup(&[&events_path, &session_out]);
}

/// Exit 7 is decided after the loop returned an answer: the Session ran its
/// Turns and replied, then `session.finished` reports the failure.
#[tokio::test]
async fn a_search_blocked_session_ends_with_exit_7_after_the_loop() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let tools = walled_web().await;
    point_env_at(&spawn_server(vec![
        tools_body(
            vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
            "",
        ),
        text_body("I found nothing on this topic.\n\n## Findings\n\n- No sources found.\n"),
    ]));
    let (req, events_path, session_out) = request_with_events("events-blocked");

    let err = run_research_with(req, tools).await.expect_err("walled");
    assert!(matches!(err, ResearchError::SearchBlocked(_)), "{err:?}");

    let events = read_events(&events_path);
    assert_eq!(
        types(&events),
        [
            "session.started",
            "turn.started",
            "model.replied",
            "turn.started",
            "model.replied",
            "session.finished",
        ]
    );
    let end = finished(&events);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("search_blocked"), Some(7))
    );
    assert_eq!(end["turns_used"], 2);
    cleanup(&[&events_path, &session_out]);
}

/// An explicit `--session-out` that cannot be written fails after the answer
/// (exit 6); the Session still closes with `session.finished`.
#[tokio::test]
async fn an_explicit_session_out_failure_ends_with_exit_6() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    point_env_at(&spawn_server(vec![text_body(
        "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust.\n",
    )]));
    let (mut req, events_path, session_out) = request_with_events("events-io");
    let blocker = temp_session_out("events-io-blocker");
    std::fs::write(&blocker, b"not a directory").expect("blocker writes");
    req.session_out = Some(blocker.join("deeper").join("session.jsonl"));

    let err = run_research_with(req, tools).await.expect_err("unwritable");
    assert!(matches!(err, ResearchError::Io(_)), "{err:?}");

    let events = read_events(&events_path);
    let end = finished(&events);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("session_io"), Some(6))
    );
    assert_eq!(end["turns_used"], 1);
    cleanup(&[&events_path, &session_out, &blocker]);
}

/// Once the Session id is known, even a configuration failure is a Session
/// with an end: `session.started` is absent, `session.finished` is not.
#[tokio::test]
async fn a_configuration_failure_after_the_session_id_still_finishes() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    std::env::set_var("GATEWAY_API_KEY", "dummy");
    let (mut req, events_path, session_out) = request_with_events("events-config");
    req.goal = "   ".to_owned();

    let err = run_research_with(req, tools).await.expect_err("empty goal");
    assert!(matches!(err, ResearchError::NotConfigured(_)), "{err:?}");

    let events = read_events(&events_path);
    assert_eq!(types(&events), ["session.finished"]);
    let end = finished(&events);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("not_configured"), Some(2))
    );
    assert_eq!(end["turns_used"], 0);
    cleanup(&[&events_path, &session_out]);
}

/// An invalid `--session-id` has no Session to name: exit 2 and no Events
/// (the output is not even created).
#[tokio::test]
async fn an_invalid_session_id_emits_no_events() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let (mut req, events_path, session_out) = request_with_events("events-bad-id");
    req.session_id = Some("../escape".to_owned());

    let err = run_research_with(req, tools).await.expect_err("bad id");
    assert!(matches!(err, ResearchError::NotConfigured(_)), "{err:?}");
    assert!(!events_path.exists(), "no Session, no Events output");
    cleanup(&[&session_out]);
}

/// An `--events` path that cannot be opened is a configuration error raised
/// before any gateway call.
#[tokio::test]
async fn an_unopenable_events_path_exits_2_before_any_gateway_call() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let (base, server) = spawn_capturing_server(vec![text_body("unused")]);
    point_env_at(&base);
    let blocker = temp_session_out("events-open-blocker");
    std::fs::write(&blocker, b"not a directory").expect("blocker writes");
    let mut req = hermetic_request("events-open");
    req.events = Some(EventsSink::File(blocker.join("events.ndjson")));
    let session_out = req.session_out.clone().expect("session out set");

    let err = run_research_with(req, tools).await.expect_err("unopenable");
    match err {
        ResearchError::NotConfigured(reason) => assert!(reason.contains("events"), "{reason}"),
        other => panic!("expected NotConfigured, got {other:?}"),
    }
    assert_eq!(server.hits(), 0, "no gateway call");
    cleanup(&[&blocker, &session_out]);
}

#[cfg(unix)]
#[tokio::test]
async fn the_events_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    point_env_at(&spawn_server(vec![text_body(
        "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust.\n",
    )]));
    let (req, events_path, session_out) = request_with_events("events-mode");

    run_research_with(req, tools)
        .await
        .expect("session succeeds");

    let mode = std::fs::metadata(&events_path)
        .expect("events file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    cleanup(&[&events_path, &session_out]);
}
