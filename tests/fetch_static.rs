//! Hermetic suite for `Fetcher`: static path against a local HTTP server,
//! Obscura fallback against `tests/fixtures/obscura-fake.sh`.
//!
//! Route names avoid the fixture's dispatch words (`blocked`, `slow`, `fail`, …)
//! so a fallback always succeeds with the fixture's default body.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use web_agent_research::web::fetch::tool::fetch_tool;
use web_agent_research::web::fetch::{FetchError, FetchPath, Fetcher, Obscura};

/// Must match `Fetcher::with_obscura`'s default body cap (5 MiB); there is no
/// public constant to import, and no reason to expose one for a single test.
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

fn fixture_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("obscura-fake.sh")
}

fn fetcher() -> Fetcher {
    Fetcher::with_obscura(Obscura::new(fixture_binary(), Duration::from_secs(10)))
}

fn fetcher_without_obscura() -> Fetcher {
    Fetcher::with_obscura(Obscura::new(
        PathBuf::from("/nonexistent/obscura"),
        Duration::from_secs(1),
    ))
}

fn article_html() -> String {
    let para = "Obscura is a headless browser written in Rust for scraping and agents. ".repeat(6);
    format!(
        "<html><head><title>A</title><script>track()</script></head><body>\
         <nav>Home | Docs</nav><h1>Article</h1><p>{para}</p>\
         <p><a href=\"https://example.org/ref\">reference</a></p><footer>(c)</footer></body></html>"
    )
}

/// Minimal Cloudflare-style interstitial matching `CHALLENGE_MARKERS`.
fn challenge_body() -> String {
    "<html><title>Just a moment...</title><div class=\"cf-chl-widget\"></div></html>".to_owned()
}

/// A response body, wired as either a length-prefixed or chunked HTTP entity.
enum Body {
    Plain(String),
    /// Sent as real HTTP/1.1 chunked transfer coding: no `Content-Length`.
    Chunked(Vec<u8>),
}

/// One HTTP response the fixture server can serve, keyed by path.
struct RouteResponse {
    status: u16,
    content_type: &'static str,
    extra_headers: &'static [(&'static str, &'static str)],
    body: Body,
}

/// Route → response. Route names avoid the Obscura fixture's dispatch words
/// (`blocked`, `slow`, `fail`, `empty`, `robots`, `bignum`, `pad`) so a
/// fallback (when one is expected) always succeeds with the fixture's
/// default body, and a *missing* fallback (when none is expected) is
/// provable: the fixture would have served success for the same path.
fn route(path: &str) -> RouteResponse {
    match path {
        "/article" => RouteResponse {
            status: 200,
            content_type: "text/html; charset=utf-8",
            extra_headers: &[],
            body: Body::Plain(article_html()),
        },
        "/notes.txt" => RouteResponse {
            status: 200,
            content_type: "text/plain",
            extra_headers: &[],
            body: Body::Plain("plain text notes ".repeat(20)),
        },
        "/shell" => RouteResponse {
            status: 200,
            content_type: "text/html",
            extra_headers: &[],
            body: Body::Plain(
                "<html><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>"
                    .to_owned(),
            ),
        },
        "/cf" => RouteResponse {
            status: 200,
            content_type: "text/html",
            extra_headers: &[],
            body: Body::Plain(challenge_body()),
        },
        "/binary" => RouteResponse {
            status: 200,
            content_type: "application/pdf",
            extra_headers: &[],
            body: Body::Plain("%PDF-1.7".to_owned()),
        },
        // 403 with a plain (non-challenge) body: no challenge marker, no
        // cf-mitigated header. Must NOT fall back.
        "/denied" => RouteResponse {
            status: 403,
            content_type: "text/html",
            extra_headers: &[],
            body: Body::Plain("<h1>Forbidden</h1>".to_owned()),
        },
        // 503 with a plain body: a real outage, not a challenge. Must NOT
        // fall back.
        "/outage" => RouteResponse {
            status: 503,
            content_type: "text/html",
            extra_headers: &[],
            body: Body::Plain("<h1>Service Unavailable</h1>".to_owned()),
        },
        // 403 whose body carries a challenge marker: must fall back.
        "/cf403" => RouteResponse {
            status: 403,
            content_type: "text/html",
            extra_headers: &[],
            body: Body::Plain(challenge_body()),
        },
        // 429 whose body carries a challenge marker: must fall back.
        "/cf429" => RouteResponse {
            status: 429,
            content_type: "text/html",
            extra_headers: &[],
            body: Body::Plain(challenge_body()),
        },
        // 403 with a plain body but the Cloudflare `cf-mitigated: challenge`
        // header: the header alone must trigger the fallback.
        "/cf-header" => RouteResponse {
            status: 403,
            content_type: "text/html",
            extra_headers: &[("cf-mitigated", "challenge")],
            body: Body::Plain("<h1>Forbidden</h1>".to_owned()),
        },
        // 200 success, no Content-Length, real chunked transfer coding,
        // body above the cap: the streamed reader must abort mid-stream.
        "/oversize-chunked" => RouteResponse {
            status: 200,
            content_type: "text/plain",
            extra_headers: &[],
            body: Body::Chunked(vec![b'a'; MAX_BODY_BYTES + 1024]),
        },
        _ => RouteResponse {
            status: 404,
            content_type: "text/html",
            extra_headers: &[],
            body: Body::Plain("<h1>Not Found</h1>".to_owned()),
        },
    }
}

/// Real HTTP/1.1 chunked transfer coding: fixed-size chunks + terminator.
fn chunk_encode(body: &[u8]) -> Vec<u8> {
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut out = Vec::with_capacity(body.len() + (body.len() / CHUNK_SIZE + 1) * 8 + 8);
    for chunk in body.chunks(CHUNK_SIZE) {
        out.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// One-connection-per-request HTTP/1.1 server on 127.0.0.1:0.
async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let response = route(&path);
                let mut head = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: {}\r\n",
                    response.status, response.content_type
                );
                for (name, value) in response.extra_headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                let wire_body = match &response.body {
                    Body::Plain(text) => {
                        head.push_str(&format!("Content-Length: {}\r\n", text.len()));
                        text.as_bytes().to_vec()
                    }
                    Body::Chunked(bytes) => {
                        head.push_str("Transfer-Encoding: chunked\r\n");
                        chunk_encode(bytes)
                    }
                };
                head.push_str("Connection: close\r\n\r\n");
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&wire_body).await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn static_html_becomes_markdown_without_browser() {
    let base = serve().await;
    let (page, path) = fetcher().fetch(&format!("{base}/article")).await.unwrap();
    assert_eq!(path, FetchPath::Static);
    assert_eq!(page.url, format!("{base}/article"));
    assert!(page.markdown.starts_with("# Article"), "{}", page.markdown);
    assert!(page
        .markdown
        .contains("[reference](https://example.org/ref)"));
    for chrome in ["Home | Docs", "track()", "(c)"] {
        assert!(!page.markdown.contains(chrome), "{chrome} leaked");
    }
}

#[tokio::test]
async fn plain_text_passes_through_static() {
    let base = serve().await;
    let (page, path) = fetcher().fetch(&format!("{base}/notes.txt")).await.unwrap();
    assert_eq!(path, FetchPath::Static);
    assert!(page.markdown.starts_with("plain text notes"));
}

#[tokio::test]
async fn unusable_static_results_fall_back_to_browser() {
    let base = serve().await;
    for route in ["/shell", "/cf", "/cf403", "/cf429", "/cf-header", "/binary"] {
        let url = format!("{base}{route}");
        let (page, path) = fetcher()
            .fetch(&url)
            .await
            .unwrap_or_else(|err| panic!("{route}: {err}"));
        assert_eq!(path, FetchPath::Browser, "{route}");
        assert_eq!(
            page.markdown,
            format!("# Title\n\nbody for {url}"),
            "{route}"
        );
    }
}

#[tokio::test]
async fn not_found_is_http_error_without_fallback() {
    let base = serve().await;
    // The fixture would succeed for this URL, so an error proves no fallback ran.
    let err = fetcher()
        .fetch(&format!("{base}/nothing-here"))
        .await
        .unwrap_err();
    let FetchError::Http { detail, .. } = &err else {
        panic!("expected Http, got {err}");
    };
    assert_eq!(detail, "HTTP 404");
}

#[tokio::test]
async fn challenge_gated_status_without_marker_is_http_error_without_fallback() {
    let base = serve().await;
    for (route, code) in [("/denied", 403), ("/outage", 503)] {
        // The fixture would succeed for these URLs, so an Http error proves no fallback ran.
        let err = fetcher()
            .fetch(&format!("{base}{route}"))
            .await
            .unwrap_err();
        let FetchError::Http { detail, .. } = &err else {
            panic!("{route}: expected Http, got {err}");
        };
        assert_eq!(detail, &format!("HTTP {code}"), "{route}");
    }
}

#[tokio::test]
async fn chunked_body_over_cap_aborts_without_content_length() {
    let base = serve().await;
    let err = fetcher()
        .fetch(&format!("{base}/oversize-chunked"))
        .await
        .unwrap_err();
    let FetchError::Http { detail, .. } = &err else {
        panic!("expected Http, got {err}");
    };
    assert!(
        detail.starts_with("body exceeds"),
        "expected size-cap detail, got {detail}"
    );
}

#[tokio::test]
async fn fallback_without_obscura_is_typed_error_naming_the_trigger() {
    let base = serve().await;
    let err = fetcher_without_obscura()
        .fetch(&format!("{base}/cf403"))
        .await
        .unwrap_err();
    let FetchError::FallbackUnavailable { reason, .. } = &err else {
        panic!("expected FallbackUnavailable, got {err}");
    };
    assert_eq!(reason, "HTTP 403");
}

#[tokio::test]
async fn static_success_never_needs_obscura() {
    let base = serve().await;
    let (_, path) = fetcher_without_obscura()
        .fetch(&format!("{base}/article"))
        .await
        .unwrap();
    assert_eq!(path, FetchPath::Static);
}

#[tokio::test]
async fn fetch_tool_wraps_markdown_in_evidence() {
    let base = serve().await;
    let url = format!("{base}/article");
    let evidence = fetch_tool(&serde_json::json!({ "url": url }), &fetcher())
        .await
        .unwrap();
    assert_eq!(evidence.source_url, url);
    assert!(evidence.markdown.starts_with("# Article"));
    assert!(evidence.collected_at.ends_with('Z'));
    assert_eq!(evidence.fetch_path, Some(FetchPath::Static));
}

#[tokio::test]
async fn fetch_tool_records_browser_fetch_path_on_fallback() {
    let base = serve().await;
    let url = format!("{base}/shell");
    let evidence = fetch_tool(&serde_json::json!({ "url": url }), &fetcher())
        .await
        .unwrap();
    assert_eq!(evidence.fetch_path, Some(FetchPath::Browser));
}

#[tokio::test]
async fn fetch_tool_rejects_non_http_urls_before_io() {
    let err = fetch_tool(&serde_json::json!({ "url": "ftp://x" }), &fetcher())
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::InvalidUrl { .. }), "{err}");
}
