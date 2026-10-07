//! Engine-aware soft deadline (#104) and the single transport retry
//! (#103), through the real fan-out call site with a paced (non-hermetic)
//! `Governor`. Real time, not a paused clock: the legs talk to loopback
//! stubs over real sockets, which a paused clock would race.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::web::search::fanout::search_multi_with_bases;
use crate::web::search::governor::test_support::EnvGuard;
use crate::web::search::governor::Governor;
use crate::web::search::test_support::{ddg_rows, sp_home_form, sp_rows, FanoutStub, StubServer};
use crate::web::search::types::{SearchInput, SearchProvider, SearchProviderError};

const UNREACHABLE: &str = "http://127.0.0.1:9/";

fn brave_page() -> String {
    r#"<html><body><div id="results"><div class="result-wrapper"><div class="result-content"><a href="https://example.com/brave" class="l1"><div class="title search-snippet-title">Brave row</div></a><div class="generic-snippet"><div class="content">b</div></div></div></div></div></body></html>"#.to_string()
}

#[tokio::test]
async fn soft_deadline_waits_for_paced_legs_in_flight_and_cancels_queued_ones() {
    // Brave: concurrency 1, a fixed 2.4 s gap, 0.9 s per response. Its
    // four legs start at 0, 2.4, 4.8 and 7.2 s. DuckDuckGo answers at
    // once, so the soft deadline (5 s) fires with a success in hand: the
    // third Brave leg (on the wire since 4.8 s) must be awaited and
    // finish after 5 s; the fourth (still queued) is cancelled unsent.
    let _guard = EnvGuard::set(&[
        ("SEARCH_ENGINES", "brave,duckduckgo"),
        ("SEARCH_PACE_BRAVE_MIN_MS", "2400"),
        ("SEARCH_PACE_BRAVE_MAX_MS", "2400"),
        ("SEARCH_PACE_DUCKDUCKGO_MIN_MS", "0"),
        ("SEARCH_PACE_DUCKDUCKGO_MAX_MS", "0"),
    ]);
    let brave = StubServer::serve_slow_body(Duration::from_millis(900), brave_page()).await;
    let (ddg, _) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows("https://example.com/ddg", "DDG row", "d"),
        sp_home_form(),
        sp_rows("https://example.com/sp", "SP", "s"),
        false,
    ))
    .await;
    let queries = ["q one", "q two", "q three", "q four"]
        .map(str::to_string)
        .to_vec();
    let input = SearchInput::validate(queries, Some(20), None).expect("valid");
    let started = tokio::time::Instant::now();
    let out = search_multi_with_bases(
        &reqwest::Client::new(),
        input,
        &format!("{ddg}/html/"),
        UNREACHABLE,
        UNREACHABLE,
        &format!("{}/search", brave.base()),
        UNREACHABLE,
        UNREACHABLE,
        UNREACHABLE,
        &Governor::new(None),
        None,
    )
    .await
    .expect("partial success");
    let elapsed = started.elapsed();
    assert!(
        elapsed > Duration::from_millis(5_400),
        "the in-flight Brave leg must be awaited past the soft deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "the queued leg must not run: {elapsed:?}"
    );
    assert_eq!(brave.hits("/search"), 3, "the 4th Brave leg was never sent");
    assert_eq!(
        out.engine_status,
        ["brave: 3 rows, timed out", "duckduckgo: 4 rows"],
        "{:?}",
        out.errors
    );
    let brave_timeouts = out
        .errors
        .iter()
        .filter(|err| {
            matches!(err, SearchProviderError::Timeout { provider: SearchProvider::Brave, query } if query == "q four")
        })
        .count();
    assert_eq!(brave_timeouts, 1, "{:?}", out.errors);
}

/// A Bing stub that drops the first `drops` connections without a
/// response (a transport failure: no HTTP status), then answers every
/// later one with one organic row. Returns its base URL and request count.
async fn flaky_bing(drops: usize) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stub bind");
    let addr = listener.local_addr().expect("stub addr");
    let count = Arc::new(AtomicUsize::new(0));
    let count_task = count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = count_task.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                if seen < drops {
                    return;
                }
                let body = r#"<ol id="b_results"><li class="b_algo"><h2><a href="https://example.com/bing">Bing row</a></h2><div class="b_caption"><p>b</p></div></li></ol>"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{body}",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
            });
        }
    });
    (format!("http://{addr}/search"), count)
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("war-fanout-{tag}-{}-{nanos}", std::process::id()))
}

async fn bing_only(
    bing: &str,
    governor: &Governor,
) -> Result<crate::web::search::types::SearchOutput, SearchProviderError> {
    let input = SearchInput::validate(vec!["q".to_string()], Some(5), None).expect("valid");
    search_multi_with_bases(
        &reqwest::Client::new(),
        input,
        UNREACHABLE,
        UNREACHABLE,
        UNREACHABLE,
        UNREACHABLE,
        UNREACHABLE,
        bing,
        bing,
        governor,
        None,
    )
    .await
}

#[tokio::test]
async fn a_transport_error_is_retried_once_and_never_suspends() {
    let _guard = EnvGuard::set(&[
        ("SEARCH_ENGINES", "bing"),
        ("SEARCH_PACE_BING_MIN_MS", "0"),
        ("SEARCH_PACE_BING_MAX_MS", "0"),
    ]);
    let root = temp_root("transport-retry");
    let governor = Governor::new(Some(root.clone()));

    // One dropped connection: the retry answers.
    let (bing, requests) = flaky_bing(1).await;
    let out = bing_only(&bing, &governor)
        .await
        .expect("the retry succeeds");
    assert_eq!(requests.load(Ordering::SeqCst), 2, "exactly one retry");
    assert_eq!(out.results.len(), 1);
    assert!(governor.suspended_remaining(SearchProvider::Bing).is_none());

    // Two dropped connections: the leg fails after its single retry, as
    // a statusless `Upstream`, and still suspends nothing.
    let (bing, requests) = flaky_bing(2).await;
    let err = bing_only(&bing, &governor)
        .await
        .expect_err("both attempts fail");
    assert_eq!(requests.load(Ordering::SeqCst), 2, "one retry, not more");
    assert!(
        matches!(
            &err,
            SearchProviderError::AllFailed {
                all_challenged: false,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        governor.suspended_remaining(SearchProvider::Bing).is_none(),
        "a transport failure must not suspend the engine"
    );
    let _ = std::fs::remove_dir_all(&root);
}
