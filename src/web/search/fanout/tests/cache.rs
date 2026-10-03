//! Per-leg cache behavior through the fan-out call site (#67).

use crate::web::search::cache::test_support::EnvGuard;
use crate::web::search::fanout::search_multi_with_bases;
use crate::web::search::governor::Governor;
use crate::web::search::test_support::{ddg_rows, sp_home_form, sp_rows, FanoutStub, StubServer};
use crate::web::search::types::SearchInput;

#[tokio::test]
async fn repeat_search_with_cache_root_makes_zero_http_requests() {
    // Acceptance (#67): a second identical `search_multi_with_bases`
    // call against the same `cache_root` must not touch the network at
    // all -- proven here by the local stub's per-route hit counter
    // staying flat across the second call, not just by asserting the
    // returned rows match. Holds the same env lock as `cache::tests`:
    // this test depends on `SEARCH_CACHE`/`SEARCH_CACHE_TTL_SECS`/
    // `SEARCH_CACHE_MAX_BYTES` staying at their defaults, which those
    // tests mutate in the same process.
    let _guard = EnvGuard::lock();
    let root = std::env::temp_dir().join(format!(
        "war-fanout-cache-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).expect("create temp cache root");
    let shared = "https://example.com/cached";
    let (base, hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows(shared, "Cached DDG", "ddg text"),
        sp_home_form(),
        sp_rows(shared, "Cached SP", "sp text"),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let make_input =
        || SearchInput::validate(vec!["cached query".to_string()], Some(15), None).expect("valid");
    let first = search_multi_with_bases(
        &client,
        make_input(),
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &Governor::hermetic(),
        Some(&root),
    )
    .await
    .expect("first call succeeds and populates the cache");
    assert!(
        !first.results.is_empty(),
        "first call must actually hit the stub"
    );
    let html_hits_after_first = hits
        .lock()
        .expect("hits")
        .get("/html/")
        .copied()
        .unwrap_or(0);
    let sp_hits_after_first = hits
        .lock()
        .expect("hits")
        .get("/sp/search")
        .copied()
        .unwrap_or(0);
    assert!(
        html_hits_after_first > 0,
        "DDG leg must have hit the stub once"
    );
    assert!(
        sp_hits_after_first > 0,
        "Startpage leg must have hit the stub once"
    );
    let second = search_multi_with_bases(
        &client,
        make_input(),
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &Governor::hermetic(),
        Some(&root),
    )
    .await
    .expect("second call is served entirely from cache");
    assert_eq!(
        hits.lock()
            .expect("hits")
            .get("/html/")
            .copied()
            .unwrap_or(0),
        html_hits_after_first,
        "a second identical search must make zero DDG HTTP requests"
    );
    assert_eq!(
        hits.lock()
            .expect("hits")
            .get("/sp/search")
            .copied()
            .unwrap_or(0),
        sp_hits_after_first,
        "a second identical search must make zero Startpage HTTP requests"
    );
    assert_eq!(
        second.results, first.results,
        "cached rows must round-trip identically"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn challenged_leg_is_never_cached() {
    // Acceptance (#67): "a Challenge is never cached." Drives the real
    // fan-out call site (not a reimplemented `if let Ok` in the test)
    // against a stub that Challenge-walls both legs: a second call
    // must hit the stub again, and the cache directory must end up
    // with no entries, because `store` is inside `if let Ok(rows) =
    // &outcome` at the leg closure and a Challenge is an `Err`.
    let _guard = EnvGuard::lock();
    let root = std::env::temp_dir().join(format!(
        "war-fanout-challenge-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&root).expect("create temp cache root");
    let (base, hits) = StubServer::serve_routes(FanoutStub::single_page(
        r#"<div id="anomaly-modal"></div>"#.to_string(),
        sp_home_form(),
        r#"<script id="anubis_challenge" type="application/json">{}</script>"#.to_string(),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let make_input =
        || SearchInput::validate(vec!["walled query".to_string()], Some(15), None).expect("valid");
    let first_err = search_multi_with_bases(
        &client,
        make_input(),
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &Governor::hermetic(),
        Some(&root),
    )
    .await
    .expect_err("both legs Challenge -> AllFailed, nothing merges");
    assert_eq!(first_err.code(), "all_failed");
    let html_hits_after_first = hits
        .lock()
        .expect("hits")
        .get("/html/")
        .copied()
        .unwrap_or(0);
    assert!(
        html_hits_after_first > 0,
        "the DDG leg must have hit the stub"
    );
    // The cache must hold no entry for the Challenged leg.
    let search_dir = root.join("search");
    let entry_count = std::fs::read_dir(&search_dir)
        .map(|read_dir| read_dir.flatten().count())
        .unwrap_or(0);
    assert_eq!(
        entry_count, 0,
        "a Challenge leg must never write a cache entry"
    );
    // A second call must hit the stub again -- proof by absence of a
    // cache short-circuit, not merely by inspecting the directory.
    let second_err = search_multi_with_bases(
        &client,
        make_input(),
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &format!("{base}/search"),
        &Governor::hermetic(),
        Some(&root),
    )
    .await
    .expect_err("still Challenge-walled, no cache to short-circuit it");
    assert_eq!(second_err.code(), "all_failed");
    assert!(
        hits.lock()
            .expect("hits")
            .get("/html/")
            .copied()
            .unwrap_or(0)
            > html_hits_after_first,
        "a Challenge leg must never be served from cache: the second call must hit the stub again"
    );
    std::fs::remove_dir_all(&root).ok();
}
