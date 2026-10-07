//! Merge across Queries and engines, and the Ok / partial / AllFailed shape
//! of the result.

use crate::web::search::fanout::search_multi_with_bases;
use crate::web::search::governor::Governor;
use crate::web::search::test_support::{ddg_rows, sp_home_form, sp_rows, FanoutStub, StubServer};
use crate::web::search::types::{SearchInput, SearchProviderError};

#[tokio::test]
async fn fanout_merges_across_queries_and_providers() {
    // 3 queries x 2 providers over the local stub with overlapping URLs:
    // one group with 2 providers + 3 queries, stats.legs == 6.
    let shared = "https://example.com/shared";
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows(shared, "Shared DDG", "ddg text"),
        sp_home_form(),
        sp_rows(shared, "Shared SP", "sp text"),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(
        vec![
            "q one".to_string(),
            "q two".to_string(),
            "q three".to_string(),
        ],
        Some(15),
        None,
    )
    .expect("valid");
    let out = search_multi_with_bases(
        &client,
        input,
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &Governor::hermetic(),
        None,
    )
    .await
    .expect("partial overlap still merges");
    // Hermetic `Governor` enables all five providers (#66): DDG +
    // Startpage still merge the shared URL; Brave/Yahoo/Bing each hit
    // this stub's unrecognized `/search` route (404 -> `Upstream`) on
    // every query, adding 9 errors atop the original merge.
    assert_eq!(out.stats.legs, 15);
    assert_eq!(out.stats.queries, 3);
    assert_eq!(
        out.errors.len(),
        9,
        "3 queries x 3 unreachable engines (Brave/Yahoo/Bing): {:?}",
        out.errors
    );
    let top = out
        .results
        .iter()
        .find(|r| r.canonical_url == "example.com/shared")
        .expect("shared group");
    assert_eq!(top.providers.len(), 2);
    assert_eq!(top.queries.len(), 3);
    assert_eq!(top.hit_count, 6);
    assert_eq!(out.stats.raw_hits, 6);
}

#[tokio::test]
async fn fanout_duplicate_queries_keep_separate_legs() {
    // Duplicate query strings are legal input (`validate` does not
    // dedupe): both legs must report hits, not collapse into one slot
    // with a phantom `Timeout` on the other.
    let shared = "https://example.com/shared";
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows(shared, "Shared DDG", "ddg text"),
        sp_home_form(),
        sp_rows(shared, "Shared SP", "sp text"),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(
        vec!["same query".to_string(), "same query".to_string()],
        Some(15),
        None,
    )
    .expect("valid");
    let out = search_multi_with_bases(
        &client,
        input,
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &Governor::hermetic(),
        None,
    )
    .await
    .expect("duplicate queries still merge");
    // 2 duplicate queries x 5 hermetic engines: DDG+Startpage succeed
    // (4 legs, 4 raw hits); Brave/Yahoo/Bing each 404 on the
    // unrecognized `/search` route for both queries (6 errors).
    assert_eq!(out.stats.legs, 10);
    assert_eq!(out.stats.raw_hits, 4);
    assert_eq!(
        out.errors.len(),
        6,
        "2 queries x 3 unreachable engines (Brave/Yahoo/Bing): {:?}",
        out.errors
    );
    let top = out
        .results
        .iter()
        .find(|r| r.canonical_url == "example.com/shared")
        .expect("shared group");
    assert_eq!(top.hit_count, 4);
}

#[tokio::test]
async fn fanout_partial_failure_still_ok() {
    // One failing leg (Startpage POST 500) still yields Ok with 1 error
    // per Query (2 SP legs fail, 2 DDG legs succeed — one SP error per
    // query).
    let shared = "https://example.com/shared";
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows(shared, "Shared DDG", "ddg text"),
        sp_home_form(),
        "unused".to_string(),
        true,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(
        vec!["q one".to_string(), "q two".to_string()],
        Some(15),
        None,
    )
    .expect("valid");
    let out = search_multi_with_bases(
        &client,
        input,
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &Governor::hermetic(),
        None,
    )
    .await
    .expect("partial success is Ok");
    // 2 queries x 5 hermetic engines: Startpage fails (500) both times;
    // Brave/Yahoo/Bing each 404 on the unrecognized `/search` route
    // both times; DDG succeeds both times.
    assert_eq!(out.stats.legs, 10);
    assert_eq!(
        out.errors.len(),
        8,
        "one SP error per query plus 3 unreachable engines per query: {:?}",
        out.errors
    );
    assert!(out.errors.iter().all(|e| e.http_status() == 503));
    assert!(!out.results.is_empty());
}

#[tokio::test]
async fn fanout_all_fail_returns_all_failed_503() {
    // Every leg fails (DDG 404-route, Startpage POST 500) AND nothing
    // merges -> Err(AllFailed) with 503.
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        "unused".to_string(),
        sp_home_form(),
        "unused".to_string(),
        true,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(vec!["q one".to_string()], Some(15), None).expect("valid");
    // Point DDG at a missing route so it 404s -> Upstream.
    let err = search_multi_with_bases(
        &client,
        input,
        &format!("{base}/missing/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &Governor::hermetic(),
        None,
    )
    .await
    .expect_err("all legs fail -> AllFailed");
    assert_eq!(err.http_status(), 503);
    assert_eq!(err.code(), "all_failed");
    match &err {
        SearchProviderError::AllFailed { all_challenged, .. } => {
            assert!(
                !all_challenged,
                "a 404 Upstream leg mixed with a 500 Upstream leg is not all-Challenge"
            );
        }
        other => panic!("expected AllFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn fanout_all_challenged_marks_all_failed() {
    // Every leg reports a bot-wall Challenge (DDG anomaly body, Startpage
    // Anubis PoW body via the search POST) AND nothing merges -> Err
    // (AllFailed) with `all_challenged: true` (#53): the signal
    // `research::agent_loop` uses to surface `SearchBlocked` to the
    // Harness instead of a silent empty Synthesis. Scoped to
    // `duckduckgo,startpage` via a non-hermetic `Governor` (#66): the
    // hermetic seam's `enabled_providers()` always returns all five
    // regardless of `SEARCH_ENGINES` (by design, #64), and Brave/
    // Yahoo/Bing legs against this stub's unrecognized `/search` route
    // would be plain `Upstream` 404s, not Challenge -- diluting this
    // test's specific "every enabled engine was Challenge-walled"
    // acceptance. A plain (non-hermetic) `Governor::new(None)` reads
    // `SEARCH_ENGINES` for real, narrowing which legs actually run,
    // while still never persisting (no cache root) or pacing (zero
    // real requests before this call sets any last-request time).
    let _guard = crate::web::search::governor::test_support::EnvGuard::lock();
    std::env::set_var("SEARCH_ENGINES", "duckduckgo,startpage");
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        r#"<div id="anomaly-modal"></div>"#.to_string(),
        sp_home_form(),
        r#"<script id="anubis_challenge" type="application/json">{}</script>"#.to_string(),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(vec!["q one".to_string()], Some(15), None).expect("valid");
    let governor = Governor::new(None);
    let err = search_multi_with_bases(
        &client,
        input,
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        &governor,
        None,
    )
    .await
    .expect_err("both legs Challenge -> AllFailed");
    assert_eq!(err.http_status(), 503);
    assert_eq!(err.code(), "all_failed");
    match &err {
        SearchProviderError::AllFailed { all_challenged, .. } => {
            assert!(all_challenged, "both legs were Challenge, must be true");
        }
        other => panic!("expected AllFailed, got {other:?}"),
    }
}
