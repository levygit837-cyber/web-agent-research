//! Hermetic `run_research_with` runs over a canned gateway and a local web:
//! Evidence and Session persistence, both wire formats, cache accounting,
//! and the search-blocked exit.

use super::super::*;
use super::helpers::{
    hermetic_request, local_web, long_page_web, point_env_at, spawn_capturing_server, spawn_server,
    temp_session_out, text_body, tool_call, tools_body, walled_web, wire_body_of,
};
use crate::research::synthesis::SynthesisSize;
use crate::test_support::EnvGuard;

/// #80: the Session is the record. A page read in three parts is
/// persisted once, whole, from the call that fetched it; the two
/// continuation parts add no row and no duplicate response URL.
#[tokio::test]
async fn session_holds_the_whole_long_page_once_not_its_parts() {
    let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
    let (tools, page_url, pages) = long_page_web().await;
    let fetch_call = |id: &str, part: Option<u32>| {
        let args = match part {
            Some(part) => serde_json::json!({ "url": page_url, "part": part }),
            None => serde_json::json!({ "url": page_url }),
        };
        tools_body(vec![tool_call(id, "fetch", &args.to_string())], "")
    };
    let final_answer = format!(
        "The page lists numbered paragraphs.\n\n## Findings\n\n- Paragraph 0999 is the last, per [Long]({page_url}).\n"
    );
    let base_url = spawn_server(vec![
        fetch_call("c1", None),
        fetch_call("c2", Some(2)),
        fetch_call("c3", Some(3)),
        text_body(&final_answer),
    ]);
    point_env_at(&base_url);
    let req = hermetic_request("long-page");
    let session_out = req.session_out.clone().expect("session out set");
    let response = run_research_with(req, tools)
        .await
        .expect("long-page run must succeed");

    assert_eq!(response.turns_used, 4);
    assert_eq!(
        response.evidence_urls,
        vec![page_url.clone()],
        "three parts of one page are one source"
    );
    assert_eq!(pages.hits("/"), 1, "parts 2 and 3 come from memory");

    let content = std::fs::read_to_string(&session_out).expect("session file must exist");
    let pages_persisted: Vec<String> = content
        .lines()
        .skip(1)
        .flat_map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).expect("row is JSON");
            row["evidence"].as_array().cloned().unwrap_or_default()
        })
        .map(|evidence| {
            assert_eq!(evidence["source_url"], page_url.as_str());
            evidence["markdown"].as_str().unwrap_or_default().to_owned()
        })
        .collect();
    assert_eq!(pages_persisted.len(), 1, "one Evidence row per URL");
    let markdown = &pages_persisted[0];
    assert!(markdown.contains("Paragraph 0000"), "first paragraph");
    assert!(markdown.contains("Paragraph 0999"), "last paragraph");
    assert!(
        !markdown.contains("[truncated") && !markdown.contains("call fetch with"),
        "the record is the page, not what the model saw"
    );
    let _ = std::fs::remove_file(&session_out);
}

#[tokio::test]
async fn hermetic_run_uses_real_tools_and_persists_fetched_evidence() {
    let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
    let (tools, page_url) = local_web().await;
    let final_answer = format!(
        "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust per [Obscura]({page_url}).\n"
    );
    let base_url = spawn_server(vec![
        tools_body(
            vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
            "",
        ),
        tools_body(
            vec![tool_call(
                "c2",
                "fetch",
                &serde_json::json!({ "url": page_url }).to_string(),
            )],
            "fetching now",
        ),
        text_body(&final_answer),
    ]);
    point_env_at(&base_url);
    let req = hermetic_request("a1");
    let session_out = req.session_out.clone().expect("session out set");
    let response = run_research_with(req, tools)
        .await
        .expect("hermetic run must succeed");

    assert_eq!(response.session_id, "test-a1");
    assert_eq!(response.turns_used, 3);
    // Search Hits are not Evidence: only the fetched page is.
    assert_eq!(response.evidence_urls, vec![page_url.clone()]);
    assert!(response
        .synthesis
        .citations
        .iter()
        .any(|citation| citation.url == page_url));
    assert_eq!(response.usage.total_tokens, 6);
    assert!(render(&response).contains(&page_url));

    let content = std::fs::read_to_string(&session_out).expect("session file must exist");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 4, "header + 3 turn rows, got {content}");
    assert!(
        lines[2].contains("headless browser written in Rust"),
        "turn 2 must persist fetched markdown: {}",
        lines[2]
    );
    assert!(
        lines[2].contains("\"fetch_path\":\"static\""),
        "fetched Evidence must record which engine produced it: {}",
        lines[2]
    );
    assert!(
        !lines[1].contains("\"source_url\""),
        "search turn has no Evidence"
    );
    let _ = std::fs::remove_file(&session_out);
}

#[tokio::test]
async fn anthropic_format_run_replays_thinking_and_sums_cache_reads() {
    let _guard = EnvGuard::lock(vec![
        "GATEWAY_BASE_URL",
        "GATEWAY_API_KEY",
        "GATEWAY_MODEL",
        "GATEWAY_API_FORMAT",
    ]);
    let (tools, page_url) = local_web().await;
    let final_answer = format!(
        "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust per [Obscura]({page_url}).\n"
    );
    let fetch_turn = serde_json::json!({
        "type": "message",
        "content": [
            {"type": "thinking", "thinking": "fetch it", "signature": "sig-1"},
            {"type": "tool_use", "id": "toolu_1", "name": "fetch", "input": {"url": page_url}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 3,
                  "cache_creation_input_tokens": 4200, "cache_read_input_tokens": 0}
    })
    .to_string();
    let answer_turn = serde_json::json!({
        "type": "message",
        "content": [{"type": "text", "text": final_answer}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 50, "output_tokens": 40,
                  "cache_creation_input_tokens": 60, "cache_read_input_tokens": 4200}
    })
    .to_string();
    let (base_url, requests) = spawn_capturing_server(vec![fetch_turn, answer_turn]);
    point_env_at(&base_url);
    std::env::set_var("GATEWAY_API_FORMAT", "anthropic");
    let req = hermetic_request("anthropic");
    let session_out = req.session_out.clone().expect("session out set");
    let response = run_research_with(req, tools)
        .await
        .expect("hermetic anthropic run must succeed");

    assert_eq!(response.turns_used, 2);
    assert_eq!(response.evidence_urls, vec![page_url.clone()]);
    assert_eq!(response.usage.cached_prompt_tokens, 4200);
    assert_eq!(response.usage.cache_creation_prompt_tokens, 4260);
    assert_eq!(response.usage.prompt_tokens, 4205 + 4310);

    let requests = requests.requests();
    assert!(
        requests[0].starts_with("POST /v1/messages "),
        "{}",
        requests[0]
    );
    let second = wire_body_of(&requests[1]);
    let messages = second["messages"].as_array().expect("messages");
    let assistant = &messages[messages.len() - 2];
    assert_eq!(
        assistant["content"][0],
        serde_json::json!({"type": "thinking", "thinking": "fetch it", "signature": "sig-1"}),
        "thinking block must be replayed verbatim first: {second}"
    );
    assert_eq!(assistant["content"][1]["type"], "tool_use");
    let results = &messages[messages.len() - 1];
    assert_eq!(results["content"][0]["type"], "tool_result");
    assert_eq!(results["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(
        results["content"][0]["cache_control"]["type"], "ephemeral",
        "the newest block carries the moving breakpoint"
    );
    let _ = std::fs::remove_file(&session_out);
}

#[tokio::test]
async fn cached_prompt_tokens_sum_across_turns_from_both_wire_shapes() {
    let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
    let (tools, page_url) = local_web().await;
    // Turn 1 reports the OpenAI shape, turn 2 the Anthropic/LiteLLM shape.
    let with_usage = |mut body: serde_json::Value, usage: serde_json::Value| {
        body["usage"] = usage;
        body.to_string()
    };
    let search_turn: serde_json::Value = serde_json::from_str(&tools_body(
        vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
        "",
    ))
    .expect("json");
    let final_turn: serde_json::Value = serde_json::from_str(&text_body(&format!(
        "Obscura is a Rust headless browser.\n\n## Findings\n\n- See [Obscura]({page_url}).\n"
    )))
    .expect("json");
    let base_url = spawn_server(vec![
        with_usage(
            search_turn,
            serde_json::json!({
                "prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                "prompt_tokens_details": {"cached_tokens": 64}
            }),
        ),
        with_usage(
            final_turn,
            serde_json::json!({
                "prompt_tokens": 200, "completion_tokens": 7, "total_tokens": 207,
                "cache_read_input_tokens": 96
            }),
        ),
    ]);
    point_env_at(&base_url);
    let req = hermetic_request("cache-sum");
    let session_out = req.session_out.clone().expect("session out set");
    let response = run_research_with(req, tools)
        .await
        .expect("hermetic run must succeed");

    assert_eq!(response.turns_used, 2);
    assert_eq!(response.usage.cached_prompt_tokens, 160);
    assert_eq!(response.usage.prompt_tokens, 300);
    let content = std::fs::read_to_string(&session_out).expect("session file must exist");
    let last = content.lines().last().expect("final turn row");
    assert!(
        last.contains("\"cached_prompt_tokens\":160"),
        "final Session row must carry the run total: {last}"
    );
    let _ = std::fs::remove_file(&session_out);
}

#[tokio::test]
async fn prompt_cache_key_is_the_session_id_generated_before_the_loop_and_stable_across_turns() {
    let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
    let (tools, page_url) = local_web().await;
    let final_answer = format!(
        "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust per [Obscura]({page_url}).\n"
    );
    let (base_url, requests) = spawn_capturing_server(vec![
        tools_body(
            vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
            "",
        ),
        tools_body(
            vec![tool_call(
                "c2",
                "fetch",
                &serde_json::json!({ "url": page_url }).to_string(),
            )],
            "fetching now",
        ),
        text_body(&final_answer),
    ]);
    point_env_at(&base_url);
    // No explicit `session_id`: forces `generate_session_id()`, which
    // must run BEFORE the loop so every turn's wire body carries it.
    let req = ResearchRequest {
        goal: "What is the Obscura headless browser?".to_owned(),
        size: SynthesisSize::Small,
        max_turns: 8,
        session_id: None,
        session_out: Some(temp_session_out("cache-key-stable")),
    };
    let session_out = req.session_out.clone().expect("session out set");
    let response = run_research_with(req, tools)
        .await
        .expect("hermetic run must succeed");

    assert_eq!(response.turns_used, 3);
    assert!(
        !response.session_id.is_empty(),
        "auto-generated session_id must be non-empty"
    );

    let bodies: Vec<serde_json::Value> = requests
        .requests()
        .iter()
        .map(|r| wire_body_of(r))
        .collect();
    assert_eq!(bodies.len(), 3, "must have captured all 3 turns");
    for (turn, body) in bodies.iter().enumerate() {
        assert_eq!(
            body["prompt_cache_key"],
            serde_json::json!(response.session_id),
            "turn {} prompt_cache_key must equal the run's session_id, got {body}",
            turn + 1
        );
    }

    let _ = std::fs::remove_file(&session_out);
}

/// Acceptance (#53): a FINAL answer with zero fetched Evidence, when
/// every search call this run was fully Challenge-walled and no search
/// call ever returned a Hit, must surface as `ResearchError::SearchBlocked`
/// (exit 7 in `main.rs`), never a silent exit-0 "I found nothing"
/// Synthesis (the bug reported in #53).
#[tokio::test]
async fn fully_walled_search_surfaces_search_blocked_not_empty_synthesis() {
    let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
    let tools = walled_web().await;
    let base_url = spawn_server(vec![
        tools_body(
            vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
            "",
        ),
        text_body("I found nothing on this topic.\n\n## Findings\n\n- No sources found.\n"),
    ]);
    point_env_at(&base_url);
    let req = hermetic_request("walled");
    let session_out = req.session_out.clone().expect("session out set");
    let err = run_research_with(req, tools)
        .await
        .expect_err("a fully walled search must not exit as a successful empty Synthesis");
    match &err {
        ResearchError::SearchBlocked(detail) => {
            assert!(
                detail.contains("startpage") && detail.contains("duckduckgo"),
                "detail must carry both engines' challenge messages: {detail}"
            );
        }
        other => panic!("expected SearchBlocked, got {other:?}"),
    }
    let _ = std::fs::remove_file(&session_out);
}

/// A search call returning at least one Hit, even if the model still
/// answers "no results" in its FINAL text, must NOT be reclassified as
/// `SearchBlocked`: the search itself worked, the model just chose not
/// to fetch anything.
#[tokio::test]
async fn hits_without_fetch_is_not_search_blocked() {
    let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
    let (tools, _page_url) = local_web().await;
    let base_url = spawn_server(vec![
        tools_body(
            vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
            "",
        ),
        text_body("No relevant sources found.\n\n## Findings\n\n- Nothing conclusive.\n"),
    ]);
    point_env_at(&base_url);
    let req = hermetic_request("hits-no-fetch");
    let session_out = req.session_out.clone().expect("session out set");
    let response = run_research_with(req, tools)
        .await
        .expect("a search that returned Hits must finalize as Ok, not SearchBlocked");
    assert!(response.evidence_urls.is_empty());
    let _ = std::fs::remove_file(&session_out);
}
