//! `recency` window filtering of legs (#81).

use crate::web::search::fanout::search_multi_with_bases;
use crate::web::search::governor::Governor;
use crate::web::search::test_support::StubReply;
use crate::web::search::test_support::{ddg_rows, sp_home_form, sp_rows, FanoutStub, StubServer};
use crate::web::search::types::{Recency, SearchProvider};
use crate::web::search::types::{SearchInput, SearchProviderError};

/// #81 acceptance: `recency: day` reaches the supporting engines with
/// their window param and sends zero requests to Brave and Bing. The
/// Yahoo stub only serves rows for a request carrying `btf=d`, so a
/// leg that dropped the window would come back empty.
#[tokio::test]
async fn recency_day_sends_btf_to_yahoo_and_skips_brave_and_bing() {
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows("https://example.com/d", "DDG", "ddg text"),
        sp_home_form(),
        sp_rows("https://example.com/d", "SP", "sp text"),
        false,
    ))
    .await;
    let brave_stub = StubServer::serve(|_: &str, _: &str| StubReply::text(404, "nope")).await;
    let yahoo_stub = StubServer::serve(|path: &str, _: &str| {
            if path.contains("btf=d") {
                StubReply::text(
                    200,
                    r#"<div class="algo-sr"><div class="compTitle"><a href="https://example.com/yahoo-day"><h3>Y</h3></a></div><div class="compText"><p>yahoo text</p></div></div>"#,
                )
            } else {
                StubReply::text(404, "unfiltered request")
            }
        })
        .await;
    let bing_stub = StubServer::serve(|_: &str, _: &str| StubReply::text(404, "nope")).await;
    let client = reqwest::Client::new();
    let input =
        SearchInput::validate(vec!["q".to_string()], Some(15), Some(Recency::Day)).expect("valid");
    let out = search_multi_with_bases(
        &client,
        input,
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &brave_stub.base(),
        &yahoo_stub.base(),
        &bing_stub.base(),
        &bing_stub.base(),
        &Governor::hermetic(),
        None,
    )
    .await
    .expect("DDG/Startpage/Yahoo legs run; Brave/Bing are skipped, not failed");
    assert_eq!(
        out.stats.legs, 3,
        "day applies to DuckDuckGo, Startpage and Yahoo only"
    );
    assert!(
        out.errors.is_empty(),
        "a skip is not an error: {:?}",
        out.errors
    );
    assert!(
        out.results
            .iter()
            .any(|r| r.display_url == "https://example.com/yahoo-day"
                && r.providers.contains(&SearchProvider::Yahoo)),
        "Yahoo must have been asked with btf=d: {:?}",
        out.results
    );
    assert_eq!(brave_stub.hits("/"), 0, "Brave gets zero requests");
    assert_eq!(bing_stub.hits("/"), 0, "Bing gets zero requests");
}

/// #81 acceptance: `recency: year` skips Yahoo too (its `btf` has no
/// year value), on top of the always-skipped Brave/Bing; only
/// DuckDuckGo and Startpage apply `year`.
#[tokio::test]
async fn recency_year_skips_yahoo_too() {
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows("https://example.com/y", "DDG", "ddg text"),
        sp_home_form(),
        sp_rows("https://example.com/y", "SP", "sp text"),
        false,
    ))
    .await;
    let brave_stub = StubServer::serve(|_: &str, _: &str| StubReply::text(404, "nope")).await;
    let yahoo_stub = StubServer::serve(|_: &str, _: &str| StubReply::text(404, "nope")).await;
    let bing_stub = StubServer::serve(|_: &str, _: &str| StubReply::text(404, "nope")).await;
    let client = reqwest::Client::new();
    let input =
        SearchInput::validate(vec!["q".to_string()], Some(15), Some(Recency::Year)).expect("valid");
    let out = search_multi_with_bases(
        &client,
        input,
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &brave_stub.base(),
        &yahoo_stub.base(),
        &bing_stub.base(),
        &bing_stub.base(),
        &Governor::hermetic(),
        None,
    )
    .await
    .expect("DDG/Startpage legs run; Brave/Yahoo/Bing are skipped, not failed");
    assert_eq!(
        out.stats.legs, 2,
        "year applies to DuckDuckGo and Startpage only"
    );
    assert_eq!(yahoo_stub.hits("/"), 0, "Yahoo's btf has no year value");
    assert_eq!(brave_stub.hits("/"), 0);
    assert_eq!(bing_stub.hits("/"), 0);
}

/// #81 acceptance: when every enabled engine is unable to apply the
/// requested window, the call makes zero HTTP requests and returns a
/// one-line model-facing note instead of unfiltered Hits.
#[tokio::test]
async fn recency_unsupported_by_every_enabled_engine_makes_zero_requests() {
    let _guard = crate::web::search::governor::test_support::EnvGuard::lock();
    std::env::set_var("SEARCH_ENGINES", "brave,bing");
    let client = reqwest::Client::new();
    let input =
        SearchInput::validate(vec!["q".to_string()], Some(15), Some(Recency::Day)).expect("valid");
    let governor = Governor::new(None);
    let out = search_multi_with_bases(
        &client,
        input,
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        "http://127.0.0.1:9/",
        &governor,
        None,
    )
    .await
    .expect("no leg to run is Ok, not an error");
    assert_eq!(out.stats.legs, 0, "zero legs: no HTTP request was made");
    assert!(out.results.is_empty());
    assert!(out.errors.is_empty());
    let note = out.note.expect("a one-line note instead of Hits");
    assert!(note.contains("day") && !note.contains('\n'), "{note}");
}

/// #81 review: engines skipped for `recency` were never asked, so a
/// wall on the engines that did run must not be reported as "every
/// enabled engine is walled" (exit-7 `SearchBlocked`): the same search
/// without `recency` would still reach the skipped engine. The control
/// call shows the same two walls do count as all-challenged when
/// nothing was narrowed.
#[tokio::test]
async fn recency_narrowed_wall_is_not_reported_as_every_engine_walled() {
    let _guard = crate::web::search::governor::test_support::EnvGuard::lock();
    std::env::set_var("SEARCH_ENGINES", "duckduckgo,brave");
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        r#"<div id="anomaly-modal"></div>"#.to_string(),
        sp_home_form(),
        String::new(),
        false,
    ))
    .await;
    let brave_stub =
        StubServer::serve(|_: &str, _: &str| StubReply::text(429, "rate limited")).await;
    let client = reqwest::Client::new();
    let mut walled = Vec::new();
    for recency in [None, Some(Recency::Day)] {
        let input = SearchInput::validate(vec!["q".to_string()], Some(15), recency).expect("valid");
        let err = search_multi_with_bases(
            &client,
            input,
            &format!("{base}/html/"),
            &format!("{base}/"),
            &format!("{base}/sp/search"),
            &brave_stub.base(),
            "http://127.0.0.1:9/",
            "http://127.0.0.1:9/",
            "http://127.0.0.1:9/",
            &Governor::new(None),
            None,
        )
        .await
        .expect_err("every leg that ran is walled");
        match err {
            SearchProviderError::AllFailed { all_challenged, .. } => walled.push(all_challenged),
            other => panic!("expected AllFailed, got {other:?}"),
        }
    }
    assert_eq!(
        walled,
        vec![true, false],
        "unnarrowed: all engines walled; recency-narrowed: Brave was never asked"
    );
}

/// #81 acceptance: with no `recency` set, every enabled engine still
/// runs its own leg, exactly as before this change.
#[tokio::test]
async fn no_recency_runs_every_enabled_engine() {
    let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows("https://example.com/n", "DDG", "ddg text"),
        sp_home_form(),
        sp_rows("https://example.com/n", "SP", "sp text"),
        false,
    ))
    .await;
    let client = reqwest::Client::new();
    let input = SearchInput::validate(vec!["q".to_string()], Some(15), None).expect("valid");
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
    .expect("5 legs, no leg skipped without recency");
    assert_eq!(
        out.stats.legs, 5,
        "every hermetic engine (DDG/Startpage/Brave/Yahoo/Bing) runs when recency is unset"
    );
    assert_eq!(out.note, None);
}
