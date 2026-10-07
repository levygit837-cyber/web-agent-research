//! Governor effects on a fan-out call: engine allowlist (#64), suspension
//! (#62) and the per-call query cap (#63).

use crate::web::search::fanout::search_multi_with_bases;
use crate::web::search::governor::Governor;
use crate::web::search::test_support::{ddg_rows, sp_home_form, sp_rows, FanoutStub, StubServer};
use crate::web::search::types::SearchProvider;
use crate::web::search::types::{SearchInput, SearchProviderError};

#[tokio::test]
async fn default_config_sends_zero_requests_to_startpage() {
    // Acceptance (#64): with the default config (no `SEARCH_ENGINES`
    // override), the fan-out sends zero requests to Startpage. Uses a
    // real (non-hermetic) `Governor` -- `enabled_providers()` on the
    // hermetic seam always returns both, by design (#62/#63/#64 are
    // production-path concerns, never hermetic-path ones).
    let _guard = crate::web::search::governor::test_support::EnvGuard::lock();
    let shared = "https://example.com/default-engines";
    let (base, hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows(shared, "DDG only", "ddg text"),
        sp_home_form(),
        sp_rows(shared, "SP unreachable", "sp text"),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(vec!["q one".to_string()], Some(15), None).expect("valid");
    let governor = Governor::new(None);
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
        &governor,
        None,
    )
    .await
    .expect("DDG-only success plus three unreachable-route Brave/Yahoo/Bing legs is still Ok");
    // #66: the default set is now `duckduckgo,brave,yahoo,bing` (4
    // engines), Startpage still opt-in only. Brave/Yahoo/Bing hit this
    // stub's unrecognized `/search` route (404 -> `Upstream`).
    assert_eq!(
        out.stats.legs, 4,
        "default enables DuckDuckGo + Brave + Yahoo + Bing"
    );
    assert_eq!(
        out.errors.len(),
        3,
        "Brave/Yahoo/Bing all hit the unrecognized /search route: {:?}",
        out.errors
    );
    assert!(
        hits.lock()
            .expect("hits")
            .get("/html/")
            .copied()
            .unwrap_or(0)
            > 0,
        "DDG must have been hit"
    );
    assert_eq!(
        hits.lock().expect("hits").get("/").copied().unwrap_or(0),
        0,
        "Startpage homepage GET must never be requested by default"
    );
    assert_eq!(
        hits.lock()
            .expect("hits")
            .get("/sp/search")
            .copied()
            .unwrap_or(0),
        0,
        "Startpage search POST must never be requested by default"
    );
}

#[tokio::test]
async fn all_suspended_fanout_yields_all_failed_with_zero_requests() {
    // Acceptance (#62): if every enabled engine is suspended, the
    // search fails with a typed `AllFailed { all_challenged: true }`
    // (the exit-7 `SearchBlocked` path's trigger, `research::run.rs`)
    // without any HTTP request -- proven by the stub's hit count
    // staying at 0 for both routes, not merely by the error shape.
    let _guard = crate::web::search::governor::test_support::EnvGuard::lock();
    std::env::set_var("SEARCH_ENGINES", "duckduckgo,startpage");
    let (base, hits) = StubServer::serve_routes(FanoutStub::single_page(
        "unreachable".to_string(),
        sp_home_form(),
        "unreachable".to_string(),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(vec!["q one".to_string()], Some(15), None).expect("valid");
    let governor = Governor::new(None);
    // Pre-suspend both enabled engines via a live Challenge, exactly
    // the state #62 persists after a real bot-wall response.
    for provider in [SearchProvider::DuckDuckGo, SearchProvider::Startpage] {
        governor.record_failure(
            provider,
            &SearchProviderError::Challenge {
                provider,
                detail: "walled".to_string(),
            },
        );
    }
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
    .expect_err("both engines suspended -> AllFailed");
    assert_eq!(err.code(), "all_failed");
    match &err {
        SearchProviderError::AllFailed {
            all_challenged,
            failures,
        } => {
            assert!(
                all_challenged,
                "a Suspended skip counts as a bot-wall signal, same as a live Challenge"
            );
            assert!(
                failures.contains("duckduckgo") && failures.contains("startpage"),
                "failures must name both suspended engines: {failures}"
            );
        }
        other => panic!("expected AllFailed, got {other:?}"),
    }
    assert_eq!(
        hits.lock()
            .expect("hits")
            .get("/html/")
            .copied()
            .unwrap_or(0),
        0,
        "a suspended DDG leg must never hit the stub"
    );
    assert_eq!(
        hits.lock().expect("hits").get("/").copied().unwrap_or(0),
        0,
        "a suspended Startpage leg must never hit the stub's homepage"
    );
    assert_eq!(
        hits.lock()
            .expect("hits")
            .get("/sp/search")
            .copied()
            .unwrap_or(0),
        0,
        "a suspended Startpage leg must never hit the stub's search route"
    );
}

#[tokio::test]
async fn per_call_query_cap_limits_requests_to_one_engine() {
    // #63: "cap on Queries dispatched to one engine within a call".
    // 3 Queries, cap 2 -> the 3rd Query's leg never reaches the stub
    // (a `Throttled` error, not a bot-wall signal), while the first
    // two do. Zero pacing delay: this proves the cap, not the gap, so
    // it must not add real wall-clock wait to the suite.
    let _guard = crate::web::search::governor::test_support::EnvGuard::lock();
    std::env::set_var("SEARCH_ENGINES", "duckduckgo,startpage");
    std::env::set_var("SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL", "2");
    std::env::set_var("SEARCH_PACE_MIN_MS", "0");
    std::env::set_var("SEARCH_PACE_MAX_MS", "0");
    let shared = "https://example.com/capped";
    let (base, hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows(shared, "DDG", "ddg text"),
        sp_home_form(),
        sp_rows(shared, "SP", "sp text"),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(
        vec!["q1".to_string(), "q2".to_string(), "q3".to_string()],
        Some(15),
        None,
    )
    .expect("valid");
    let governor = Governor::new(None);
    let out = search_multi_with_bases(
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
    .expect("partial success: 2 of 3 queries per engine succeed");
    assert_eq!(
        out.errors.len(),
        2,
        "the 3rd query's leg on each engine (DDG + Startpage) must be capped: {:?}",
        out.errors
    );
    assert!(
        out.errors
            .iter()
            .all(|e| matches!(e, SearchProviderError::Throttled { .. })),
        "a capped leg is Throttled, not a bot-wall signal: {:?}",
        out.errors
    );
    // Each engine sees exactly 2 requests (q1, q2), never 3.
    assert_eq!(
        hits.lock()
            .expect("hits")
            .get("/html/")
            .copied()
            .unwrap_or(0),
        2,
        "DDG must be capped at 2 requests this call"
    );
    assert_eq!(
        hits.lock()
            .expect("hits")
            .get("/sp/search")
            .copied()
            .unwrap_or(0),
        2,
        "Startpage must be capped at 2 requests this call"
    );
}
