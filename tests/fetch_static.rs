//! Hermetic suite for `Fetcher`: static path against a local HTTP server,
//! Obscura fallback against `tests/fixtures/obscura-fake.sh`.
//!
//! Route names avoid the fixture's dispatch words (`blocked`, `slow`, `fail`, …)
//! so a fallback always succeeds with the fixture's default body.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use web_agent_research::web::fetch::tool::fetch_tool;
use web_agent_research::web::fetch::{EgressPolicy, FetchError, FetchPath, Fetcher, Obscura};

/// Must match `Fetcher::with_policy`'s default body cap (5 MiB); there is no
/// public constant to import, and no reason to expose one for a single test.
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

fn fixture_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("obscura-fake.sh")
}

/// The fixture server binds `127.0.0.1`, so every fetcher here opts out of
/// the egress policy explicitly (#97); the policy itself is tested in
/// `web::fetch::egress` and `web::fetch::fetcher`.
fn fetcher() -> Fetcher {
    Fetcher::with_policy(
        Obscura::new(fixture_binary(), Duration::from_secs(10)),
        EgressPolicy::AllowPrivate,
    )
}

fn fetcher_without_obscura() -> Fetcher {
    Fetcher::with_policy(
        Obscura::new(
            PathBuf::from("/nonexistent/obscura"),
            Duration::from_secs(1),
        ),
        EgressPolicy::AllowPrivate,
    )
}

fn article_html() -> String {
    let para = "Obscura is a headless browser written in Rust for scraping and agents. ".repeat(6);
    format!(
        "<html><head><title>A</title><script>track()</script></head><body>\
         <nav>Home | Docs</nav><h1>Article</h1><p>{para}</p>\
         <p><a href=\"https://example.org/ref\">reference</a></p><footer>(c)</footer></body></html>"
    )
}

/// Same article shape plus a data-URI image and a tracking-param link, to
/// prove the static path's result went through the shared cleanup pass
/// (#79): the data URI must never reach the markdown, and the tracking
/// params must be gone from the kept link.
fn noisy_article_html() -> String {
    format!(
        "<html><head><title>A</title></head><body><h1>Article</h1><p>{}</p>\
         <p><img src=\"data:image/png;base64,{}\" alt=\"inline chart\"></p>\
         <p><a href=\"https://example.org/ref?utm_source=newsletter&id=3\">reference</a></p>\
         </body></html>",
        "Obscura is a headless browser written in Rust for scraping and agents. ".repeat(6),
        "A".repeat(400),
    )
}

/// Minimal Cloudflare-style interstitial matching `CHALLENGE_MARKERS`.
fn challenge_body() -> String {
    "<html><title>Just a moment...</title><div class=\"cf-chl-widget\"></div></html>".to_owned()
}

/// A response body, wired as either a length-prefixed or chunked HTTP entity.
enum Body {
    Plain(String),
    /// Pre-encoded raw bytes sent with a normal `Content-Length` (used with
    /// a `Content-Encoding` extra header for the #57 gzip-body-cap test).
    Bytes(Vec<u8>),
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
        "/noisy-article" => RouteResponse {
            status: 200,
            content_type: "text/html; charset=utf-8",
            extra_headers: &[],
            body: Body::Plain(noisy_article_html()),
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
        // 200, gzip-compressed body + `Content-Encoding: gzip` (#57): the
        // static client must decode this via reqwest's `gzip` feature and
        // hand `Fetcher` decoded HTML, not raw compressed bytes.
        "/gzip-article" => {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(article_html().as_bytes()).unwrap();
            RouteResponse {
                status: 200,
                content_type: "text/html; charset=utf-8",
                extra_headers: &[("Content-Encoding", "gzip")],
                body: Body::Bytes(encoder.finish().unwrap()),
            }
        }
        // 200, gzip-compressed body whose *decoded* size is over the cap,
        // but whose *compressed* `Content-Length` is tiny (repeated bytes
        // compress hard): proves the cap is enforced against decoded
        // bytes streamed as they arrive, not against the wire size.
        "/gzip-bomb" => {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder
                .write_all(&vec![b'a'; MAX_BODY_BYTES + 1024])
                .unwrap();
            let compressed = encoder.finish().unwrap();
            assert!(
                compressed.len() < 65536,
                "sanity: compressed gzip-bomb body must stay well under the cap, got {}",
                compressed.len()
            );
            RouteResponse {
                status: 200,
                content_type: "text/plain",
                extra_headers: &[("Content-Encoding", "gzip")],
                body: Body::Bytes(compressed),
            }
        }
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
                    Body::Bytes(bytes) => {
                        head.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
                        bytes.clone()
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
async fn static_path_result_went_through_the_cleanup_pass() {
    // #79: proves `Fetcher`'s static path calls the shared cleanup, not
    // just that `clean_markdown` works in isolation (that's covered unit-
    // side). The data URI must never reach the markdown, and the tracked
    // link must keep its destination without the tracking param.
    let base = serve().await;
    let (page, path) = fetcher()
        .fetch(&format!("{base}/noisy-article"))
        .await
        .unwrap();
    assert_eq!(path, FetchPath::Static);
    assert!(
        !page.markdown.to_ascii_lowercase().contains("base64"),
        "data URI leaked: {}",
        page.markdown
    );
    assert!(
        page.markdown.contains("inline chart"),
        "alt text missing: {}",
        page.markdown
    );
    assert!(
        page.markdown
            .contains("[reference](https://example.org/ref?id=3)"),
        "tracking param not stripped: {}",
        page.markdown
    );
    assert!(!page.markdown.contains("utm_source"));
}

#[tokio::test]
async fn gzip_encoded_static_body_decodes_via_accept_encoding() {
    // #57: reqwest's `gzip` feature must decode the compressed body before
    // `Fetcher` ever sees it; the resulting markdown matches the same
    // article served plain.
    let base = serve().await;
    let (page, path) = fetcher()
        .fetch(&format!("{base}/gzip-article"))
        .await
        .unwrap();
    assert_eq!(path, FetchPath::Static);
    assert!(page.markdown.starts_with("# Article"), "{}", page.markdown);
    assert!(page
        .markdown
        .contains("[reference](https://example.org/ref)"));
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
async fn gzip_decoded_body_over_cap_aborts_on_decoded_size_not_wire_size() {
    // #57: with decompression enabled, the body-size cap module doc claims
    // it "always applies to decoded bytes, streamed chunk-by-chunk, never
    // to the compressed size". `/gzip-bomb` serves a wire body well under
    // the cap that decodes to well over it, so this fails unless the cap
    // is really enforced post-decode.
    let base = serve().await;
    let err = fetcher()
        .fetch(&format!("{base}/gzip-bomb"))
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

/// Captures raw request header *lines* (verbatim, wire order) for the one
/// request it serves, then replies 200 with a short HTML body.
async fn serve_capturing_headers() -> (String, std::sync::Arc<parking_lot::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: std::sync::Arc<parking_lot::Mutex<Vec<String>>> = Default::default();
    let captured_task = captured.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let captured = captured_task.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut lines = raw.split("\r\n");
                let _request_line = lines.next().unwrap_or("");
                let header_lines: Vec<String> = lines
                    .take_while(|line| !line.is_empty())
                    .map(|line| line.to_string())
                    .collect();
                *captured.lock() = header_lines;
                let body = "<html><body><h1>Hi</h1><p>Hello there, this is a long enough paragraph of static article content so the markdown produced clears the JS-shell threshold and the static path is used without a browser fallback being triggered here.</p></body></html>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), captured)
}

#[tokio::test]
async fn static_fetch_sends_coherent_chrome_navigation_headers() {
    // #74: the static fetch draws one browser profile per fetch and sends
    // it as a direct navigation (`sec-fetch-site: none`, no Referer) in
    // Chrome's real navigation header order.
    let (base, captured) = serve_capturing_headers().await;
    let (_, path) = fetcher().fetch(&base).await.unwrap();
    assert_eq!(path, FetchPath::Static);

    let lines = captured.lock().clone();
    let header_value = |name: &str| -> Option<String> {
        lines.iter().find_map(|line| {
            let (n, v) = line.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    let header_names: Vec<String> = lines
        .iter()
        .filter_map(|line| line.split_once(':'))
        .map(|(n, _)| n.trim().to_ascii_lowercase())
        .collect();

    // Chrome navigation order (#58/#74): sec-ch-ua*, upgrade-insecure-requests,
    // user-agent, accept, sec-fetch-*, accept-encoding, accept-language. No
    // Referer: this is a direct navigation.
    let expected_prefix = [
        "sec-ch-ua",
        "sec-ch-ua-mobile",
        "sec-ch-ua-platform",
        "upgrade-insecure-requests",
        "user-agent",
        "accept",
        "sec-fetch-site",
        "sec-fetch-mode",
        "sec-fetch-user",
        "sec-fetch-dest",
        "accept-encoding",
        "accept-language",
    ];
    assert_eq!(
        &header_names[..expected_prefix.len()],
        &expected_prefix[..],
        "header order mismatch: {header_names:?}"
    );
    assert!(
        !header_names.contains(&"referer".to_string()),
        "direct navigation must not send Referer: {header_names:?}"
    );

    let sec_fetch_site = header_value("sec-fetch-site").expect("sec-fetch-site present");
    assert_eq!(sec_fetch_site, "none");

    let user_agent = header_value("user-agent").expect("user-agent present");
    let sec_ch_ua = header_value("sec-ch-ua").expect("sec-ch-ua present");
    let ua_major = user_agent
        .split("Chrome/")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .expect("UA carries a Chrome/<major> token");
    assert!(
        sec_ch_ua.contains(&format!("\"Chromium\";v=\"{ua_major}\"")),
        "UA major {ua_major} not reflected in sec-ch-ua: {sec_ch_ua}"
    );
    assert!(
        sec_ch_ua.contains(&format!("\"Google Chrome\";v=\"{ua_major}\"")),
        "UA major {ua_major} not reflected in sec-ch-ua: {sec_ch_ua}"
    );
}
