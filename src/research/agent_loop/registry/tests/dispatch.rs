//! Argument validation before I/O, stub ordering, tool definitions, and the
//! search-blocked mapping.

use super::super::*;
use super::helpers::{call, kind, reason, TEST_PART_CHARS};

#[test]
fn tool_defs_come_from_web_schemas_in_registry_order() {
    let registry = ToolRegistry::offline();
    let defs = registry.tool_defs(&["fetch".to_owned(), "search".to_owned()]);
    assert_eq!(defs.len(), 2);
    assert_eq!(defs[0].name, "search");
    assert_eq!(defs[0].parameters, search_tool_schema()["parameters"]);
    assert_eq!(defs[1].name, "fetch");
    assert_eq!(defs[1].parameters, fetch_tool_schema()["parameters"]);
    assert_eq!(registry.tool_defs(&["search".to_owned()]).len(), 1);
    assert!(registry.tool_defs(&["nope".to_owned()]).is_empty());
}

#[tokio::test]
async fn stub_pops_in_order_then_exhausts() {
    let registry = ToolRegistry::stub(VecDeque::from([ToolResult::Search { hits: vec![] }]));
    assert!(matches!(
        registry
            .execute(&call("search", "{}"), TEST_PART_CHARS)
            .await,
        ToolResult::Search { .. }
    ));
    assert!(reason(
        registry
            .execute(&call("search", "{}"), TEST_PART_CHARS)
            .await
    )
    .contains("exhausted"));
}

#[tokio::test]
async fn dispatch_failures_are_classified_before_io() {
    let registry = ToolRegistry::offline();
    let unknown = registry
        .execute(&call("Search", "{}"), TEST_PART_CHARS)
        .await;
    assert_eq!(kind(&unknown), FailureKind::Dispatch);
    assert!(reason(unknown).contains("unknown tool"));
    let bad_json = registry
        .execute(&call("search", "{bad"), TEST_PART_CHARS)
        .await;
    assert_eq!(kind(&bad_json), FailureKind::Dispatch);
    assert!(reason(bad_json).contains("not valid JSON"));
    let bad_args = registry
        .execute(&call("search", r#"{"query": "x"}"#), TEST_PART_CHARS)
        .await;
    assert_eq!(kind(&bad_args), FailureKind::Dispatch);
    assert!(reason(bad_args).starts_with("invalid args for 'search'"));
    let bad_url = registry
        .execute(&call("fetch", r#"{"url": "ftp://x/y"}"#), TEST_PART_CHARS)
        .await;
    assert_eq!(kind(&bad_url), FailureKind::Dispatch);
    assert!(reason(bad_url).starts_with("invalid args for 'fetch'"));
}

#[tokio::test]
async fn unreachable_endpoints_are_execution_failures() {
    let registry = ToolRegistry::offline();
    let search = registry
        .execute(&call("search", r#"{"queries": ["x"]}"#), TEST_PART_CHARS)
        .await;
    assert_eq!(kind(&search), FailureKind::Execution);
    assert!(reason(search).starts_with("search failed"));
    let fetch = registry
        .execute(
            &call("fetch", r#"{"url": "http://127.0.0.1:9/page"}"#),
            TEST_PART_CHARS,
        )
        .await;
    assert_eq!(kind(&fetch), FailureKind::Execution);
    assert!(reason(fetch).starts_with("fetch failed"));
}

/// Regression (#53): both provider legs walled with a bot-detection
/// challenge -> `searcher.search` returns
/// `SearchProviderError::AllFailed { all_challenged: true, .. }`, and
/// `execute` must turn that into `ToolResult::SearchBlocked`, not the
/// generic `Failed`, so the runner can remember it for the whole run.
/// #66: `Searcher`'s hermetic `Governor` always enables all five
/// providers, so Brave/Yahoo/Bing each get their own tiny stub tuned to
/// their real Challenge marker (Brave/Bing: HTTP 429; Yahoo: a redirect
/// to a `guce.yahoo.com` host) so every enabled engine stays walled.
#[tokio::test]
async fn all_challenged_search_becomes_search_blocked() {
    use crate::web::search::test_support::{sp_home_form, FanoutStub, StubReply, StubServer};
    let (search_base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        r#"<div id="anomaly-modal"></div>"#.to_string(),
        sp_home_form(),
        r#"<script id="anubis_challenge" type="application/json">{}</script>"#.to_string(),
        false,
    ))
    .await;
    let brave_stub =
        StubServer::serve(|_: &str, _: &str| StubReply::text(429, "rate limited")).await;
    let yahoo_stub = StubServer::serve(|_: &str, _: &str| {
        StubReply::redirect(302, "https://guce.yahoo.com/consent")
    })
    .await;
    let bing_stub = StubServer::serve(|_: &str, _: &str| StubReply::text(429, "blocked")).await;
    let registry = ToolRegistry::new(
        Searcher::with_bases(
            &format!("{search_base}/html/"),
            &format!("{search_base}/"),
            &format!("{search_base}/sp/search"),
            &brave_stub.base(),
            &yahoo_stub.base(),
            &bing_stub.base(),
        ),
        Fetcher::with_obscura(crate::web::fetch::Obscura::new(
            "/nonexistent/obscura".into(),
            std::time::Duration::from_secs(1),
        )),
    );
    let result = registry
        .execute(&call("search", r#"{"queries": ["q"]}"#), TEST_PART_CHARS)
        .await;
    match &result {
        ToolResult::SearchBlocked { detail } => {
            assert!(
                detail.contains("startpage") && detail.contains("duckduckgo"),
                "detail must carry both engines' challenge messages: {detail}"
            );
        }
        other => panic!("expected SearchBlocked, got {other:?}"),
    }
    assert!(!result.is_success(), "SearchBlocked must not be a success");
    assert_eq!(result.failure_kind(), Some(FailureKind::Execution));
    assert!(result.render().starts_with("FAILED: search blocked:"));
}
