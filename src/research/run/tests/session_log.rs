//! The Session file as the incremental log (#150, ADR-0010 §5): what is on
//! disk when a Session ends well, ends badly, or is interrupted.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::super::*;
use super::helpers::{
    hermetic_request, local_web, point_env_at, session_rows, spawn_server, temp_session_out,
    text_body, tool_call, tools_body, turn_rows,
};
use crate::research::events::EventsSink;
use crate::research::session::{read_session_rows, SessionRow};
use crate::test_support::{CannedResponse, CannedServer, EnvGuard};

const ENV: [&str; 3] = ["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"];

fn search_call() -> String {
    tools_body(
        vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
        "",
    )
}

/// The Session file as JSON rows. Holds it to the format 2 shape: the header
/// is line 0, every row says format 2, and `event` rows number their Events
/// 1, 2, 3, … with no gaps.
fn read_v2(path: &Path) -> Vec<Value> {
    let rows = session_rows(&std::fs::read_to_string(path).expect("Session file exists"));
    assert_eq!(rows[0]["kind"], "header", "line 0 is the header");
    let mut seq = 0;
    for row in &rows {
        assert_eq!(row["format_version"], 2, "{row}");
        if row["kind"] == "event" {
            seq += 1;
            assert_eq!(row["seq"], seq, "no gap in the Event rows: {row}");
        }
    }
    // The typed reader agrees with the raw rows.
    let typed = read_session_rows(path).expect("a v2 file reads back");
    assert_eq!(typed.len(), rows.len());
    rows
}

/// `kind` and, for events, `type` of every row: the shape `jq` shows.
fn shape(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|row| match row["kind"].as_str().expect("kind") {
            "event" => format!("event:{}", row["type"].as_str().expect("type")),
            "turn" => format!("turn:{}", row["turn"]),
            other => other.to_owned(),
        })
        .collect()
}

fn finished(rows: &[Value]) -> &Value {
    let last = rows.last().expect("rows");
    assert_eq!(last["type"], "session.finished", "the file ends closed");
    &last["data"]
}

/// A Session that answers leaves header first, `session.finished` last, and
/// the same envelopes `--events` printed, with the `turn` rows between.
#[tokio::test]
async fn a_successful_session_leaves_a_v2_file_that_mirrors_the_events() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, page_url) = local_web().await;
    let fetch_args = serde_json::json!({ "url": page_url }).to_string();
    point_env_at(&spawn_server(vec![
        search_call(),
        tools_body(vec![tool_call("c2", "fetch", &fetch_args)], ""),
        text_body(&format!(
            "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust per [Obscura]({page_url}).\n"
        )),
    ]));
    let mut req = hermetic_request("log-ok");
    let events_path = temp_session_out("log-ok-events");
    req.events = Some(EventsSink::File(events_path.clone()));
    let session_out = req.session_out.clone().expect("session out set");

    run_research_with(req, tools).await.expect("answers");

    let rows = read_v2(&session_out);
    assert_eq!(
        shape(&rows),
        [
            "header",
            "event:session.started",
            "event:turn.started",
            "event:model.replied",
            "event:search.started",
            "event:search.finished",
            "turn:1",
            "event:turn.started",
            "event:model.replied",
            "event:fetch.started",
            "event:fetch.finished",
            "turn:2",
            "event:turn.started",
            "event:model.replied",
            "event:answer.verified",
            "turn:3",
            "event:session.finished",
        ]
    );
    assert_eq!(finished(&rows)["outcome"], "ok");
    // The final `turn` row carries the Synthesis and the run usage, as in v1.
    let turns = turn_rows(&std::fs::read_to_string(&session_out).unwrap());
    assert_eq!(turns[0]["synthesis"], Value::Null);
    assert_eq!(turns[1]["evidence"][0]["source_url"], page_url.as_str());
    assert_eq!(turns[2]["synthesis"]["size"], "small");
    assert_eq!(turns[2]["usage"]["total_tokens"], 6);
    // Each `event` row is the `--events` envelope plus `kind` and
    // `format_version`, so both views of a Session agree line by line.
    let streamed: Vec<Value> = std::fs::read_to_string(&events_path)
        .expect("events file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("envelope"))
        .collect();
    let logged: Vec<Value> = rows
        .iter()
        .filter(|row| row["kind"] == "event")
        .cloned()
        .map(|mut row| {
            let object = row.as_object_mut().expect("row is an object");
            object.remove("kind");
            object.remove("format_version");
            row
        })
        .collect();
    assert_eq!(logged, streamed);
    let _ = std::fs::remove_file(&events_path);
    let _ = std::fs::remove_file(&session_out);
}

/// Exit 9: the Turn that completed has its row, the Turn that hit the
/// deadline has its Events, and the file ends with `session.finished`.
#[tokio::test]
async fn a_deadline_session_keeps_its_completed_turns_and_closes() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let server = CannedServer::spawn(vec![
        CannedResponse::text(200, search_call()),
        CannedResponse::text(200, text_body("too late")).with_delay(Duration::from_secs(4)),
    ]);
    point_env_at(&format!("{}/v1", server.base()));
    let mut req = hermetic_request("log-deadline");
    req.deadline_secs = 2;
    let session_out = req.session_out.clone().expect("session out set");

    let err = run_research_with(req, tools)
        .await
        .expect_err("deadline passes");
    assert!(
        matches!(err, ResearchError::DeadlineExceeded { .. }),
        "{err:?}"
    );

    let rows = read_v2(&session_out);
    assert_eq!(
        shape(&rows),
        [
            "header",
            "event:session.started",
            "event:turn.started",
            "event:model.replied",
            "event:search.started",
            "event:search.finished",
            "turn:1",
            "event:turn.started",
            "event:session.finished",
        ]
    );
    let end = finished(&rows);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("deadline_exceeded"), Some(9))
    );
    let _ = std::fs::remove_file(&session_out);
}

/// Exit 8 on the second Turn: the first Turn's row and the failure survive.
#[tokio::test]
async fn a_rejected_session_keeps_its_completed_turns_and_closes() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let server = CannedServer::spawn(vec![
        CannedResponse::text(200, search_call()),
        CannedResponse::text(401, "{}"),
    ]);
    point_env_at(&format!("{}/v1", server.base()));
    let req = hermetic_request("log-rejected");
    let session_out = req.session_out.clone().expect("session out set");

    let err = run_research_with(req, tools)
        .await
        .expect_err("401 is final");
    assert!(matches!(err, ResearchError::GatewayRejected(_)), "{err:?}");

    let rows = read_v2(&session_out);
    assert_eq!(
        shape(&rows),
        [
            "header",
            "event:session.started",
            "event:turn.started",
            "event:model.replied",
            "event:search.started",
            "event:search.finished",
            "turn:1",
            "event:turn.started",
            "event:session.finished",
        ]
    );
    let end = finished(&rows);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("gateway_rejected"), Some(8))
    );
    assert_eq!(end["turns_used"], 2);
    let _ = std::fs::remove_file(&session_out);
}

/// `ctrl_c` mid-Session (a pre-resolved interrupt stands for the signal):
/// the Session ends `Interrupted` with exit 130, and the file closes with
/// `session.finished` saying so.
#[tokio::test]
async fn an_interrupted_session_closes_its_file_with_exit_130() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let server = CannedServer::spawn(vec![
        CannedResponse::text(200, text_body("never read")).with_delay(Duration::from_secs(5))
    ]);
    point_env_at(&format!("{}/v1", server.base()));
    let mut req = hermetic_request("log-interrupted");
    let events_path = temp_session_out("log-interrupted-events");
    req.events = Some(EventsSink::File(events_path.clone()));
    let session_out = req.session_out.clone().expect("session out set");

    let err = run_research_until(req, tools, std::future::ready(()))
        .await
        .expect_err("interrupted");
    assert!(matches!(err, ResearchError::Interrupted), "{err:?}");
    assert_eq!(err.exit_code(), 130);

    let rows = read_v2(&session_out);
    assert_eq!(
        shape(&rows),
        [
            "header",
            "event:session.started",
            "event:turn.started",
            "event:session.finished",
        ]
    );
    let end = finished(&rows);
    assert_eq!(
        (end["outcome"].as_str(), end["exit_code"].as_u64()),
        (Some("interrupted"), Some(130))
    );
    // `--events` closes the same way: one `session.finished`, from one place.
    let streamed = std::fs::read_to_string(&events_path).expect("events file");
    let last: Value = serde_json::from_str(streamed.lines().last().unwrap()).unwrap();
    assert_eq!(last["data"]["outcome"], "interrupted");
    let _ = std::fs::remove_file(&events_path);
    let _ = std::fs::remove_file(&session_out);
}

/// A Session whose answer is ready is not turned into an interrupt by a
/// signal that arrives with it.
#[tokio::test]
async fn an_answer_that_is_ready_wins_over_a_late_interrupt() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, _) = local_web().await;
    let session_out = {
        let req = hermetic_request("log-late-interrupt");
        let session_out = req.session_out.clone().expect("session out set");
        point_env_at(&spawn_server(vec![text_body(
            "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust.\n",
        )]));
        let interrupt = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        };
        run_research_until(req, tools, interrupt)
            .await
            .expect("the answer arrives long before the interrupt");
        session_out
    };
    let rows = read_v2(&session_out);
    assert_eq!(finished(&rows)["outcome"], "ok");
    let _ = std::fs::remove_file(&session_out);
}

/// Format 1 files (header + turn rows, no events) still load.
#[test]
fn a_v1_file_still_reads() {
    let path = temp_session_out("log-v1");
    let usage =
        r#"{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2,"reasoning_tokens":0}"#;
    let content = format!(
        "{{\"kind\":\"header\",\"format_version\":1,\"session_id\":\"old\",\"goal\":\"g\",\"size\":\"small\",\"started_at\":\"2026-09-28T23:00:46Z\"}}\n\
         {{\"kind\":\"turn\",\"format_version\":1,\"session_id\":\"old\",\"turn\":1,\"evidence\":[],\"synthesis\":null,\"usage\":{usage},\"recorded_at\":\"2026-09-28T23:00:47Z\"}}\n"
    );
    std::fs::write(&path, content).expect("fixture writes");

    let rows = read_session_rows(&path).expect("v1 loads");
    let [SessionRow::Header(header), SessionRow::Turn(turn)] = rows.as_slice() else {
        panic!("expected header + turn, got {rows:?}");
    };
    assert_eq!((header.format_version, turn.format_version), (1, 1));
    assert_eq!(turn.usage.total_tokens, 2);
    let _ = std::fs::remove_file(&path);
}
