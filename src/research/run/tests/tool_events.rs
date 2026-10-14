//! Tool and verification Events of a hermetic Session (#148, ADR-0010 §3):
//! `search.*`, `fetch.*`, `tool.failed`, `answer.verified`, with the fields
//! a Harness reads to follow the run live.

use serde_json::{json, Value};

use super::super::*;
use super::events::{cleanup, read_events, request_with_events, types};
use super::helpers::{
    local_web, local_web_governed, long_page_web, point_env_at, refusing_web, spawn_server,
    text_body, tool_call, tools_body,
};
use crate::test_support::EnvGuard;
use crate::web::search::governor::test_support as governor_env;
use crate::web::search::governor::Governor;
use crate::web::search::types::{SearchProvider, SearchProviderError};

const ENV: [&str; 3] = ["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"];

/// The Events of one `type`, in order.
fn of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == kind).collect()
}

fn engine<'a>(finished: &'a Value, name: &str) -> &'a Value {
    finished["data"]["engines"]
        .as_array()
        .expect("engines array")
        .iter()
        .find(|e| e["engine"] == name)
        .unwrap_or_else(|| panic!("no engine {name} in {finished}"))
}

#[tokio::test]
async fn one_search_and_two_fetches_emit_their_events_in_order() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, page_url) = local_web().await;
    let second_url = format!("{page_url}-two");
    let answer = format!("Obscura is a Rust browser.\n\n## Findings\n\n- Written in Rust [Obscura]({page_url}).\n- Fast [Two]({second_url}).\n");
    point_env_at(&spawn_server(vec![
        tools_body(
            vec![tool_call(
                "s1",
                "search",
                r#"{"queries": ["obscura"], "recency": "year"}"#,
            )],
            "",
        ),
        tools_body(
            vec![
                tool_call("f1", "fetch", &json!({ "url": page_url }).to_string()),
                tool_call("f2", "fetch", &json!({ "url": second_url }).to_string()),
            ],
            "",
        ),
        text_body(&answer),
    ]));
    let (req, events_path, session_out) = request_with_events("tool-events-ok");

    run_research_with(req, tools).await.expect("session ok");

    let events = read_events(&events_path);
    let tool_types: Vec<&str> = types(&events)
        .into_iter()
        .filter(|t| t.starts_with("search.") || t.starts_with("fetch.") || *t == "answer.verified")
        .collect();
    assert_eq!(
        tool_types,
        [
            "search.started",
            "search.finished",
            "fetch.started",
            "fetch.finished",
            "fetch.started",
            "fetch.finished",
            "answer.verified",
        ]
    );

    let started = &of(&events, "search.started")[0];
    assert_eq!(started["turn"], 1);
    assert_eq!(started["data"]["call_id"], "s1");
    assert_eq!(started["data"]["queries"], json!(["obscura"]));
    assert_eq!(started["data"]["recency"], "year");

    let search = of(&events, "search.finished")[0];
    assert_eq!(search["turn"], 1);
    assert_eq!(search["data"]["call_id"], "s1");
    assert_eq!(search["data"]["ok"], true);
    assert!(search["data"]["hits"].as_u64().is_some_and(|n| n >= 1));
    let ddg = engine(search, "duckduckgo");
    assert_eq!(ddg["status"], "rows");
    assert!(ddg["rows"].as_u64().is_some_and(|n| n >= 1));

    let fetch_started = of(&events, "fetch.started");
    assert_eq!(fetch_started[0]["data"]["call_id"], "f1");
    assert_eq!(fetch_started[0]["data"]["url"], json!(page_url));
    assert_eq!(fetch_started[0]["data"]["part"], Value::Null);
    assert_eq!(fetch_started[1]["data"]["call_id"], "f2");
    let fetch_finished = of(&events, "fetch.finished");
    for (finished, id, url) in [
        (fetch_finished[0], "f1", &page_url),
        (fetch_finished[1], "f2", &second_url),
    ] {
        assert_eq!(finished["turn"], 2);
        let data = &finished["data"];
        assert_eq!(data["call_id"], id);
        assert_eq!(data["ok"], true);
        assert_eq!(data["final_url"], json!(url));
        assert_eq!(data["fetch_path"], "static");
        assert_eq!(
            (data["part"].clone(), data["parts"].clone()),
            (json!(1), json!(1))
        );
        assert!(data["chars"].as_u64().is_some_and(|n| n > 0));
        assert_eq!(data["served_from"], "network");
        assert_eq!(data["error"], Value::Null);
    }
    cleanup(&[&events_path, &session_out]);
}

#[tokio::test]
async fn a_suspended_engine_shows_its_reason_and_seconds_left() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    // Startpage is opt-in; enable it so one engine can answer while
    // DuckDuckGo is suspended.
    let _engines = governor_env::EnvGuard::set(&[("SEARCH_ENGINES", "duckduckgo,startpage")]);
    let governor = Governor::new(None);
    governor.record_failure(
        SearchProvider::DuckDuckGo,
        &SearchProviderError::Challenge {
            provider: SearchProvider::DuckDuckGo,
            detail: "walled".to_owned(),
        },
    );
    let (tools, page_url) = local_web_governed(Some(governor)).await;
    point_env_at(&spawn_server(vec![
        tools_body(
            vec![tool_call("s1", "search", r#"{"queries": ["obscura"]}"#)],
            "",
        ),
        text_body(&format!(
            "Rust browser.\n\n## Findings\n\n- No fetch [x]({page_url}).\n"
        )),
    ]));
    let (req, events_path, session_out) = request_with_events("tool-events-suspended");

    let _ = run_research_with(req, tools).await;

    let events = read_events(&events_path);
    let search = of(&events, "search.finished")[0];
    assert_eq!(search["data"]["ok"], true, "the other engine still answers");
    let ddg = engine(search, "duckduckgo");
    assert_eq!(ddg["status"], "suspended");
    assert_eq!(ddg["problems"][0]["kind"], "suspended");
    assert!(ddg["problems"][0]["remaining_secs"]
        .as_u64()
        .is_some_and(|n| n > 0));
    assert!(!ddg["problems"][0]["reason"]
        .as_str()
        .unwrap_or("")
        .is_empty());
    assert_eq!(engine(search, "startpage")["status"], "rows");
    cleanup(&[&events_path, &session_out]);
}

#[tokio::test]
async fn an_egress_refused_fetch_reports_why_and_counts_the_failure() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let tools = refusing_web();
    point_env_at(&spawn_server(vec![
        tools_body(
            vec![tool_call(
                "f1",
                "fetch",
                r#"{"url": "http://127.0.0.1:1/private"}"#,
            )],
            "",
        ),
        text_body("Nothing.\n\n## Findings\n\n- None.\n"),
    ]));
    let (req, events_path, session_out) = request_with_events("tool-events-egress");

    let _ = run_research_with(req, tools).await;

    let events = read_events(&events_path);
    let finished = of(&events, "fetch.finished")[0];
    assert_eq!(finished["data"]["call_id"], "f1");
    assert_eq!(finished["data"]["ok"], false);
    assert_eq!(finished["data"]["served_from"], Value::Null);
    let reason = finished["data"]["error"].as_str().expect("error reason");
    assert!(!reason.is_empty());
    let failed = of(&events, "tool.failed")[0];
    assert_eq!(failed["turn"], 1);
    assert_eq!(failed["data"]["call_id"], "f1");
    assert_eq!(failed["data"]["tool"], "fetch");
    assert_eq!(failed["data"]["kind"], "execution");
    assert_eq!(failed["data"]["reason"], json!(reason));
    assert_eq!(failed["data"]["consecutive_failures"], 1);
    assert_eq!(failed["data"]["max_repairs"], 2);
    cleanup(&[&events_path, &session_out]);
}

#[tokio::test]
async fn a_later_part_is_served_from_memory_and_a_bad_part_is_a_dispatch_failure() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, page_url, _pages) = long_page_web().await;
    point_env_at(&spawn_server(vec![
        tools_body(
            vec![tool_call(
                "f1",
                "fetch",
                &json!({ "url": page_url }).to_string(),
            )],
            "",
        ),
        tools_body(
            vec![tool_call(
                "f2",
                "fetch",
                &json!({ "url": page_url, "part": 2 }).to_string(),
            )],
            "",
        ),
        tools_body(
            vec![tool_call(
                "f3",
                "fetch",
                &json!({ "url": page_url, "part": 99 }).to_string(),
            )],
            "",
        ),
        text_body(&format!(
            "Long.\n\n## Findings\n\n- Page [Long]({page_url}).\n"
        )),
    ]));
    let (req, events_path, session_out) = request_with_events("tool-events-parts");

    run_research_with(req, tools).await.expect("session ok");

    let events = read_events(&events_path);
    let finished = of(&events, "fetch.finished");
    assert_eq!(finished[0]["data"]["served_from"], "network");
    assert_eq!(finished[0]["data"]["part"], 1);
    assert_eq!(finished[0]["data"]["parts"], 3);
    assert_eq!(finished[1]["data"]["served_from"], "memory");
    assert_eq!(finished[1]["data"]["part"], 2);
    assert_eq!(finished[1]["data"]["parts"], 3);
    assert_eq!(finished[2]["data"]["ok"], false);
    assert_eq!(finished[2]["data"]["part"], Value::Null);
    let failed = of(&events, "tool.failed")[0];
    assert_eq!(failed["data"]["call_id"], "f3");
    assert_eq!(failed["data"]["kind"], "dispatch");
    assert_eq!(failed["data"]["consecutive_failures"], 1);
    cleanup(&[&events_path, &session_out]);
}

#[tokio::test]
async fn answer_verified_counts_match_the_response_verification() {
    let _guard = EnvGuard::lock(ENV.to_vec());
    let (tools, page_url) = local_web().await;
    let answer = format!(
        "Obscura is a Rust browser.\n\n## Findings\n\n- Obscura is a headless browser written in Rust [Obscura]({page_url}).\n- Invented claim about quantum widgets.\n"
    );
    point_env_at(&spawn_server(vec![
        tools_body(
            vec![tool_call(
                "f1",
                "fetch",
                &json!({ "url": page_url }).to_string(),
            )],
            "",
        ),
        text_body(&answer),
    ]));
    let (req, events_path, session_out) = request_with_events("tool-events-verified");

    let response = run_research_with(req, tools).await.expect("session ok");

    let events = read_events(&events_path);
    let verified = of(&events, "answer.verified");
    assert_eq!(verified.len(), 1);
    let data = &verified[0]["data"];
    let v = &response.synthesis.verification;
    assert_eq!(data["bullets"], v.bullets);
    assert_eq!(data["cited"], v.cited);
    assert_eq!(data["exact"], v.exact);
    assert_eq!(data["partial"], v.partial);
    assert_eq!(data["none"], v.none);
    assert_eq!(data["citations"], response.synthesis.citations.len());
    assert!(v.bullets >= 2, "{v:?}");
    let order = types(&events);
    let at = |t: &str| order.iter().position(|x| *x == t).expect(t);
    assert!(at("answer.verified") < at("session.finished"));
    assert!(at("model.replied") < at("answer.verified"));
    cleanup(&[&events_path, &session_out]);
}
