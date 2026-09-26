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

/// Route → (status, content type, body).
fn route(path: &str) -> (u16, &'static str, String) {
    match path {
        "/article" => (200, "text/html; charset=utf-8", article_html()),
        "/notes.txt" => (200, "text/plain", "plain text notes ".repeat(20)),
        "/shell" => (
            200,
            "text/html",
            "<html><body><div id=\"root\"></div><script src=\"/app.js\"></script></body></html>"
                .to_owned(),
        ),
        "/cf" => (
            200,
            "text/html",
            "<html><title>Just a moment...</title><div class=\"cf-chl-widget\"></div></html>"
                .to_owned(),
        ),
        "/denied" => (403, "text/html", "<h1>Forbidden</h1>".to_owned()),
        "/binary" => (200, "application/pdf", "%PDF-1.7".to_owned()),
        _ => (404, "text/html", "<h1>Not Found</h1>".to_owned()),
    }
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
                let (status, content_type, body) = route(&path);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
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
    for route in ["/shell", "/cf", "/denied", "/binary"] {
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
async fn fallback_without_obscura_is_typed_error_naming_the_trigger() {
    let base = serve().await;
    let err = fetcher_without_obscura()
        .fetch(&format!("{base}/denied"))
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
}

#[tokio::test]
async fn fetch_tool_rejects_non_http_urls_before_io() {
    let err = fetch_tool(&serde_json::json!({ "url": "ftp://x" }), &fetcher())
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::InvalidUrl { .. }), "{err}");
}
