//! The crates.io vertical in a fan-out call (#109): it joins only
//! crate-shaped Queries, its docs.rs Hit is not buried by the fusion, and
//! the per-call query cap counts only the Queries it was sent.

use crate::web::search::fanout::search_multi_with_bases;
use crate::web::search::governor::test_support::EnvGuard;
use crate::web::search::governor::Governor;
use crate::web::search::test_support::{StubReply, StubServer};
use crate::web::search::types::{SearchInput, SearchOutput, SearchProvider};

const SEARCH_BYTES: &str = include_str!("../../../../../tests/fixtures/cratesio/search-bytes.json");
const CRATE_QUERY: &str = "What does the `bytes` crate's `Buf` trait provide?";

/// Bing answers `/search` with two rows (the same blog post twice would
/// dedupe, so two distinct pages); crates.io answers `/api/v1/crates`
/// with the captured `q=bytes` fixture. Anything else is a 404.
fn routes(path: &str, _body: &str) -> StubReply {
    if path.starts_with("/api/v1/crates?") {
        StubReply::text(200, SEARCH_BYTES)
    } else if path.starts_with("/search") {
        StubReply::text(
            200,
            r#"<html><body><ol id="b_results"><li class="b_algo"><h2><a href="https://blog.example/buf">Buf explained</a></h2><p>blog</p></li><li class="b_algo"><h2><a href="https://forum.example/buf">Buf thread</a></h2><p>forum</p></li></ol></body></html>"#,
        )
    } else {
        StubReply::text(404, "nope")
    }
}

/// One fan-out over `queries` with only the engines in `engines`
/// (`SEARCH_ENGINES`), unpaced, every engine pointed at `stub`.
async fn run(stub: &StubServer, engines: &str, queries: &[&str], cap: &str) -> SearchOutput {
    let _guard = EnvGuard::set(&[
        ("SEARCH_ENGINES", engines),
        ("SEARCH_PACE_MIN_MS", "0"),
        ("SEARCH_PACE_MAX_MS", "0"),
        ("SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL", cap),
    ]);
    let base = stub.base();
    let input = SearchInput::validate(
        queries.iter().map(|q| (*q).to_string()).collect(),
        Some(15),
        None,
    )
    .expect("valid");
    search_multi_with_bases(
        &reqwest::Client::new(),
        input,
        &format!("{base}/html/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
        &format!("{base}/brave"),
        &format!("{base}/yahoo"),
        &format!("{base}/search"),
        &format!("{base}/api/v1/crates"),
        &Governor::new(None),
        None,
    )
    .await
    .expect("search succeeds")
}

#[tokio::test]
async fn crate_query_puts_the_docs_rs_hit_first() {
    let stub = StubServer::serve(routes).await;
    let out = run(&stub, "bing,cratesio", &[CRATE_QUERY], "4").await;
    assert_eq!(out.stats.legs, 2);
    let first = &out.results[0];
    assert_eq!(first.display_url, "https://docs.rs/bytes");
    assert_eq!(first.title, "bytes 1.12.1");
    assert_eq!(first.providers, vec![SearchProvider::CratesIo]);
    assert_eq!(out.results.len(), 3, "{:?}", out.results);
    assert_eq!(
        out.engine_status,
        vec!["bing: 2 rows".to_string(), "cratesio: 1 row".to_string()]
    );
    assert_eq!(stub.hits("/api/v1/crates"), 1);
}

#[tokio::test]
async fn other_queries_send_nothing_to_crates_io() {
    let stub = StubServer::serve(routes).await;
    let out = run(&stub, "bing,cratesio", &["weather in São Paulo"], "4").await;
    assert_eq!(out.stats.legs, 1, "Bing only");
    assert_eq!(out.engine_status, vec!["bing: 2 rows".to_string()]);
    assert_eq!(stub.hits("/api/v1/crates"), 0);
}

#[tokio::test]
async fn query_cap_counts_only_the_queries_the_vertical_was_sent() {
    let stub = StubServer::serve(routes).await;
    // Cap 1: the vertical's first leg is the second Query, its 1st send.
    let out = run(
        &stub,
        "cratesio",
        &["weather in São Paulo", CRATE_QUERY],
        "1",
    )
    .await;
    assert_eq!(out.stats.legs, 1);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(out.results[0].display_url, "https://docs.rs/bytes");
}

#[tokio::test]
async fn only_the_vertical_and_no_crate_query_is_a_note_not_a_failure() {
    let stub = StubServer::serve(routes).await;
    let out = run(&stub, "cratesio", &["weather in São Paulo"], "4").await;
    assert_eq!(out.stats.legs, 0);
    assert!(out.results.is_empty());
    assert!(out.note.as_deref().is_some_and(|n| n.contains("crate")));
    assert_eq!(stub.hits("/api/v1/crates"), 0);
}
