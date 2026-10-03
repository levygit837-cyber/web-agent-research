//! Hermetic suite: the static fetch path (`fetch_tool`, the same entry the
//! agent loop uses) against trimmed, verbatim excerpts of two real pages,
//! `tests/fixtures/{bbc-news,github-htmd}-excerpt.html`, captured 2026-10-03.
//!
//! The excerpts keep the real noise the #79 cleanup targets: BBC's alt-less
//! lazy-load placeholder images and a footer link carrying `utm_*` params,
//! GitHub's `camo.githubusercontent.com` image-proxy badge. Assertions are on
//! consumer-visible outcomes in `Evidence.markdown`, never a golden comparison.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use web_agent_research::web::fetch::tool::fetch_tool;
use web_agent_research::web::fetch::{FetchPath, Fetcher, Obscura};

const BBC: &str = include_str!("fixtures/bbc-news-excerpt.html");
const GITHUB: &str = include_str!("fixtures/github-htmd-excerpt.html");

/// Serves the two excerpts: `/bbc` and anything else (`/github`).
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
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let body = if path == "/bbc" { BBC } else { GITHUB };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

/// Evidence markdown for `route`, asserting it came from the static path:
/// the browser binary does not exist, so a fallback would fail the fetch.
async fn evidence_markdown(route: &str) -> String {
    let base = serve().await;
    let fetcher = Fetcher::with_obscura(Obscura::new(
        PathBuf::from("/nonexistent/obscura"),
        Duration::from_secs(1),
    ));
    let evidence = fetch_tool(&json!({ "url": format!("{base}{route}") }), &fetcher)
        .await
        .unwrap();
    assert_eq!(evidence.fetch_path, Some(FetchPath::Static));
    evidence.markdown
}

#[tokio::test]
async fn bbc_evidence_has_no_alt_less_placeholder_image() {
    let md = evidence_markdown("/bbc").await;
    assert!(
        !md.contains("grey-placeholder"),
        "alt-less placeholder image reached Evidence: {md}"
    );
    assert!(
        !md.contains("[]("),
        "empty-text link reached Evidence: {md}"
    );
}

#[tokio::test]
async fn bbc_evidence_keeps_footer_link_destination_without_tracking_params() {
    let md = evidence_markdown("/bbc").await;
    assert!(md.contains("[BritBox](https://www.britbox.com/)"), "{md}");
    for tracking in ["utm_source", "utm_medium", "utm_campaign"] {
        assert!(!md.contains(tracking), "{tracking} reached Evidence: {md}");
    }
}

#[tokio::test]
async fn bbc_evidence_keeps_headlines_prose_and_alt_text_links() {
    let md = evidence_markdown("/bbc").await;
    assert!(
        md.contains(
            "## G7 to release 100 million barrels of oil and diesel after Trump export ban threat"
        ),
        "{md}"
    );
    assert!(
        md.contains("The coordinated release is aimed at heading off further price spikes and avoiding a ban on US diesel exports."),
        "{md}"
    );
    assert!(
        md.contains("[Tennessee Department of Corrections inmate photo of Christa Pike](/news/articles/c3kgqw7zvz47o)"),
        "{md}"
    );
}

#[tokio::test]
async fn github_evidence_drops_camo_proxy_url_and_keeps_badge_alt_and_target() {
    let md = evidence_markdown("/github").await;
    assert!(
        !md.contains("camo.githubusercontent.com"),
        "image-proxy URL reached Evidence: {md}"
    );
    assert!(
        md.contains("[crates.io version](https://crates.io/crates/htmd)"),
        "{md}"
    );
}

#[tokio::test]
async fn github_evidence_keeps_prose_links_and_code_sample() {
    let md = evidence_markdown("/github").await;
    assert!(
        md.contains("An HTML to Markdown converter for Rust, inspired by [turndown.js](https://github.com/mixmark-io/turndown)."),
        "{md}"
    );
    assert!(md.contains("htmd = \"0.5\""), "code sample lost: {md}");
}
