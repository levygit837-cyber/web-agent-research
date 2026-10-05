//! Per-run fetch dedup (#43): repeat fetches, URL variants, redirects, the
//! already-fetched marker on Hits, and the full-loop acceptance run.

use super::super::*;
use super::helpers::{base_port, call, page_html, spawn_llm, TEST_PART_CHARS};
use crate::research::agent_loop::test_support::{text_body, tool_call, tools_body};

#[tokio::test]
async fn repeat_fetch_skips_network_and_returns_already_fetched() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
    let url = format!("{}/page", pages.base());
    let registry = ToolRegistry::offline();

    let first = registry
        .execute(
            &call("fetch", &serde_json::json!({ "url": url }).to_string()),
            TEST_PART_CHARS,
        )
        .await;
    let evidence_url = match first {
        ToolResult::Fetch { evidence, .. } => evidence.source_url.clone(),
        other => panic!("expected Fetch, got {other:?}"),
    };
    assert_eq!(pages.hits("/"), 1);

    let second = registry
        .execute(
            &call("fetch", &serde_json::json!({ "url": url }).to_string()),
            TEST_PART_CHARS,
        )
        .await;
    match &second {
        ToolResult::AlreadyFetched {
            url: repeated,
            first_url,
            ..
        } => {
            assert_eq!(repeated, &url);
            assert_eq!(first_url, &evidence_url);
        }
        other => panic!("expected AlreadyFetched, got {other:?}"),
    }
    assert_eq!(second.url(), None, "AlreadyFetched carries no Evidence URL");
    assert!(
        second.is_success(),
        "a skipped repeat is success, not a repair failure"
    );
    assert!(second.render("n").contains("ALREADY FETCHED"));
    assert_eq!(pages.hits("/"), 1, "repeat fetch must not reach the server");
}

#[tokio::test]
async fn www_and_trailing_slash_variants_dedup() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
    let base = pages.base();
    let port = base_port(&base);
    let first_url = format!("http://localhost:{port}/page");
    let second_url = format!("http://www.localhost:{port}/page/");
    let registry = ToolRegistry::offline();

    let first = registry
        .execute(
            &call(
                "fetch",
                &serde_json::json!({ "url": first_url }).to_string(),
            ),
            TEST_PART_CHARS,
        )
        .await;
    assert!(matches!(first, ToolResult::Fetch { .. }));
    assert_eq!(pages.hits("/"), 1);

    let second = registry
        .execute(
            &call(
                "fetch",
                &serde_json::json!({ "url": second_url }).to_string(),
            ),
            TEST_PART_CHARS,
        )
        .await;
    assert!(
        matches!(second, ToolResult::AlreadyFetched { .. }),
        "expected AlreadyFetched, got {second:?}"
    );
    assert_eq!(
        pages.hits("/"),
        1,
        "the www/trailing-slash variant must dedup before any network I/O"
    );
}

#[tokio::test]
async fn redirect_target_dedups_against_final_url() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|path: &str, _: &str| {
        if path == "/from" {
            StubReply::redirect(301, "/to")
        } else {
            StubReply::text(200, &page_html())
        }
    })
    .await;
    let from_url = format!("{}/from", pages.base());
    let to_url = format!("{}/to", pages.base());
    let registry = ToolRegistry::offline();

    let first = registry
        .execute(
            &call("fetch", &serde_json::json!({ "url": from_url }).to_string()),
            TEST_PART_CHARS,
        )
        .await;
    let final_url = match first {
        ToolResult::Fetch { evidence, .. } => evidence.source_url.clone(),
        other => panic!("expected Fetch, got {other:?}"),
    };
    assert_eq!(
        final_url, to_url,
        "Evidence.source_url must be the post-redirect URL"
    );

    let second = registry
        .execute(
            &call("fetch", &serde_json::json!({ "url": to_url }).to_string()),
            TEST_PART_CHARS,
        )
        .await;
    match second {
        ToolResult::AlreadyFetched { first_url, .. } => assert_eq!(first_url, final_url),
        other => panic!(
            "fetching the redirect target directly must dedup against the earlier final URL, got {other:?}"
        ),
    }
}

#[tokio::test]
async fn failed_fetch_does_not_record_and_allows_retry() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(404, "nope")).await;
    let url = format!("{}/missing", pages.base());
    let registry = ToolRegistry::offline();

    let first = registry
        .execute(
            &call("fetch", &serde_json::json!({ "url": url }).to_string()),
            TEST_PART_CHARS,
        )
        .await;
    assert!(matches!(first, ToolResult::Failed { .. }));
    assert_eq!(pages.hits("/"), 1);

    let second = registry
        .execute(
            &call("fetch", &serde_json::json!({ "url": url }).to_string()),
            TEST_PART_CHARS,
        )
        .await;
    assert!(
        matches!(second, ToolResult::Failed { .. }),
        "a failed fetch must not dedup a retry, got {second:?}"
    );
    assert_eq!(
        pages.hits("/"),
        2,
        "a failed fetch is never recorded, so a retry hits the server again"
    );
}

#[tokio::test]
async fn already_fetched_hit_gets_marker_others_dont() {
    use crate::web::search::test_support::{
        ddg_rows, sp_home_form, sp_rows, FanoutStub, StubReply, StubServer,
    };
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
    let url_a = format!("{}/a", pages.base());
    let url_b = "https://example.com/never-fetched".to_owned();

    let (search_base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows(&url_a, "A title", "a snippet"),
        sp_home_form(),
        sp_rows(&url_b, "B title", "b snippet"),
        false,
    ))
    .await;
    let registry = ToolRegistry::new(
        Searcher::with_bases(
            &format!("{search_base}/html/"),
            &format!("{search_base}/"),
            &format!("{search_base}/sp/search"),
            &format!("{search_base}/search"),
            &format!("{search_base}/search"),
            &format!("{search_base}/search"),
        ),
        Fetcher::with_policy(
            crate::web::fetch::Obscura::new(
                "/nonexistent/obscura".into(),
                std::time::Duration::from_secs(1),
            ),
            crate::web::fetch::EgressPolicy::AllowPrivate,
        ),
    );

    let fetched = registry
        .execute(
            &call("fetch", &serde_json::json!({ "url": url_a }).to_string()),
            TEST_PART_CHARS,
        )
        .await;
    assert!(matches!(fetched, ToolResult::Fetch { .. }));

    let searched = registry
        .execute(
            &call(
                "search",
                &serde_json::json!({ "queries": ["obscura"] }).to_string(),
            ),
            TEST_PART_CHARS,
        )
        .await;
    let rendered = searched.render("n");
    assert!(
        rendered.contains("A title (already fetched)"),
        "fetched Hit must carry the marker: {rendered}"
    );
    assert!(
        rendered.contains("B title") && !rendered.contains("B title (already fetched)"),
        "never-fetched Hit must render without the marker: {rendered}"
    );
}

/// Acceptance (#43): a hermetic full-turn run where the model requests
/// `fetch` on the same URL twice yields exactly one entry with a URL in
/// `RunReport.evidence` (the shape `run.rs`'s `evidence_urls` derives from).
#[tokio::test]
async fn full_run_dedups_repeat_fetch_into_one_evidence_url() {
    use crate::llm::{Gateway, GatewayConfig};
    use crate::research::agent_loop::{run_loop, LoopBudget, LoopInput};
    use crate::research::synthesis::SynthesisSize;
    use crate::web::search::test_support::{StubReply, StubServer};

    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
    let page_url = format!("{}/obscura", pages.base());

    let registry = ToolRegistry::new(
        Searcher::with_bases(
            "http://127.0.0.1:9/",
            "http://127.0.0.1:9/",
            "http://127.0.0.1:9/",
            "http://127.0.0.1:9/",
            "http://127.0.0.1:9/",
            "http://127.0.0.1:9/",
        ),
        Fetcher::with_policy(
            crate::web::fetch::Obscura::new(
                "/nonexistent/obscura".into(),
                std::time::Duration::from_secs(1),
            ),
            crate::web::fetch::EgressPolicy::AllowPrivate,
        ),
    );

    let llm = spawn_llm(vec![
        tools_body(
            vec![tool_call(
                "c1",
                "fetch",
                &serde_json::json!({ "url": page_url }).to_string(),
            )],
            "",
        ),
        tools_body(
            vec![tool_call(
                "c2",
                "fetch",
                &serde_json::json!({ "url": page_url }).to_string(),
            )],
            "",
        ),
        text_body(&format!(
            "Summary about {page_url}.\n\n## Findings\n\n- Point one.\n"
        )),
    ]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(
            format!("{}/v1", llm.base()),
            "test-key".to_owned(),
            "m".to_owned(),
        )
        .with_max_attempts(1),
        reqwest::Client::new(),
    );
    let input = LoopInput {
        goal: "What is the Obscura engine?".to_owned(),
        size: SynthesisSize::Medium,
        allowed_tools: vec!["fetch".to_owned()],
    };
    let report = run_loop(&gateway, &registry, &input, &LoopBudget::default())
        .await
        .expect("run must finalize");
    let evidence_urls: Vec<String> = report
        .evidence
        .iter()
        .filter_map(|e| e.url.clone())
        .collect();
    assert_eq!(evidence_urls, vec![page_url]);
}
