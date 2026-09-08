//! Parallel multi-query fan-out against a local stub (std only).
//!
//! N queries x 2 providers over a `TcpListener` stub serving canned provider
//! HTML with overlapping URLs across legs; asserts merged consensus + error
//! collection. Uses the `#[doc(hidden)]` base-URL overrides: the stub
//! contract is the specified seam (PLAN step 5), doc-hidden so it never
//! appears in rustdoc.

use web_agent_research::shared::tools::search::search_multi_with_bases;
use web_agent_research::shared::types::search::SearchInput;

fn ddg_rows(url: &str, title: &str, snippet: &str) -> String {
    format!(
        "<html><body><div class=\"result\"><h2 class=\"result__title\"><a class=\"result__a\" href=\"{url}\">{title}</a></h2><div class=\"result__snippet\">{snippet}</div></div></body></html>"
    )
}

fn sp_rows(url: &str, title: &str, snippet: &str) -> String {
    format!(
        "<html><body><div class=\"result\"><a class=\"result-link\" href=\"{url}\"><h2 class=\"wgl-title\">{title}</h2></a><p class=\"description\">{snippet}</p></div></body></html>"
    )
}

fn sp_home_form() -> String {
    "<html><body><form action=\"/sp/search\" method=\"post\"><input type=\"hidden\" name=\"sc\" value=\"tok\"/></form></body></html>".to_string()
}

struct StubRoute {
    ddg: String,
    sp_home: String,
    sp_search: String,
    sp_search_fail: bool,
}

async fn serve_stub(cfg: StubRoute) -> String {
    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("stub bind");
    let addr = listener.local_addr().expect("stub addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let (ddg, home, search, fail) = (
                cfg.ddg.clone(),
                cfg.sp_home.clone(),
                cfg.sp_search.clone(),
                cfg.sp_search_fail,
            );
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                    .await
                    .unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut lines = raw.lines();
                let request_line = lines.next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("/");
                let route = path.split('?').next().unwrap_or("/").to_string();
                let (status, body) = if method == "POST" && route == "/html/" {
                    (200u16, ddg.clone())
                } else if route == "/" || route == "/index.html" {
                    (200u16, home.clone())
                } else if route == "/sp/search" {
                    if fail {
                        (500u16, "boom".to_string())
                    } else {
                        (200u16, search.clone())
                    }
                } else {
                    (404u16, "nope".to_string())
                };
                let response = format!(
                    "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{body}",
                    reason(status),
                    body.len(),
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
            });
        }
    });
    format!("http://{addr}")
}

fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("client")
}

#[tokio::test]
async fn fanout_merges_across_queries_and_providers() {
    // Local stub serves overlapping URLs for 3 queries x 2 providers: a
    // single group with 2 providers + 3 queries, stats.legs == 6.
    let shared = "https://example.com/shared";
    let base = serve_stub(StubRoute {
        ddg: ddg_rows(shared, "Shared DDG", "ddg text"),
        sp_home: sp_home_form(),
        sp_search: sp_rows(shared, "Shared SP", "sp text"),
        sp_search_fail: false,
    })
    .await;
    let client = test_client();
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
    )
    .await
    .expect("overlap merges");
    assert_eq!(out.stats.legs, 6);
    assert_eq!(out.stats.queries, 3);
    assert_eq!(out.stats.raw_hits, 6);
    assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
    let top = out
        .results
        .iter()
        .find(|r| r.canonical_url == "example.com/shared")
        .expect("shared group");
    assert_eq!(top.providers.len(), 2);
    assert_eq!(top.queries.len(), 3);
    assert_eq!(top.hit_count, 6);
}

#[tokio::test]
async fn fanout_all_fail_returns_all_failed_503() {
    // Every leg fails (DDG missing route 404, Startpage POST 500) AND nothing
    // merges -> Err(AllFailed) with 503.
    let base = serve_stub(StubRoute {
        ddg: "unused".to_string(),
        sp_home: sp_home_form(),
        sp_search: "unused".to_string(),
        sp_search_fail: true,
    })
    .await;
    let client = test_client();
    let input = SearchInput::validate(vec!["q one".to_string()], Some(15), None).expect("valid");
    let err = search_multi_with_bases(
        &client,
        input,
        &format!("{base}/missing/"),
        &format!("{base}/"),
        &format!("{base}/sp/search"),
    )
    .await
    .expect_err("all legs fail -> AllFailed");
    assert_eq!(err.http_status(), 503);
    assert_eq!(err.code(), "all_failed");
}
