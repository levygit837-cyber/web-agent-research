//! Fetch-only web search engine: DDG + Startpage over plain HTTP.
//!
//! Small Interface (`search_multi`, five pure helpers, two `#[doc(hidden)]`
//! base-URL overrides) over provider legs (`ddg`, `startpage`), merge
//! (`dedup`), fan-out (`fanout`) and codecs (`decode`). Callers cross only
//! this root; provider forms and deadlines stay inside.

pub mod ddg;
pub mod decode;
pub mod dedup;
pub mod fanout;
pub mod startpage;
#[doc(hidden)]
pub use ddg::ddg_search_with_base;
pub use ddg::{
    is_ddg_anomaly, parse_continuation_form, parse_ddg_html, unwrap_ddg_url, DDG_HTML_URL,
    DDG_REFERER,
};
pub use dedup::{dedup_key, merge_sources};
pub use fanout::search_multi_with_bases;
pub use fanout::{all_failed_message, search_multi};
pub use startpage::startpage_search_with_base;
pub use startpage::{
    is_startpage_challenge, parse_search_form_inputs, parse_startpage_html, sanitize_startpage_url,
    STARTPAGE_HOME_URL, STARTPAGE_SEARCH_URL,
};

/// Desktop navigation header profile (port of Omp
/// `buildBrowserNavigationHeaders` without the `HeaderGenerator`: static UA +
/// `Sec-CH-UA*` + `Sec-Fetch-*` + `Accept-Language`). Shared by every leg.
pub(crate) fn apply_browser_headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    builder
        .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Sec-CH-UA", r#""Chromium";v="126", "Google Chrome";v="126", "Not-A.Brand";v="99""#)
        .header("Sec-CH-UA-Mobile", "?0")
        .header("Sec-CH-UA-Platform", r#""macOS""#)
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Site", "same-origin")
        .header("Sec-Fetch-User", "?1")
        .header("Upgrade-Insecure-Requests", "1")
}

/// Per-request timeout ceiling: `LEG_TIMEOUT_SECS` in production, 1 s under
/// `cfg(test)` so the slow-stub leg-timeout test runs in milliseconds while
/// still exercising the real `.timeout(...)` path in `ddg_post` and friends.
pub(crate) fn leg_timeout() -> std::time::Duration {
    #[cfg(test)]
    return std::time::Duration::from_secs(1);
    #[cfg(not(test))]
    return std::time::Duration::from_secs(crate::shared::types::search::LEG_TIMEOUT_SECS);
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};
    pub(crate) struct StubReply {
        status: u16,
        body: String,
    }

    impl StubReply {
        pub(crate) fn text(status: u16, body: &str) -> Self {
            StubReply {
                status,
                body: body.to_string(),
            }
        }
    }

    pub(crate) struct StubServer {
        base_url: String,
        hits: Arc<Mutex<std::collections::HashMap<String, usize>>>,
    }

    impl StubServer {
        pub(crate) fn base(&self) -> String {
            self.base_url.clone()
        }

        pub(crate) fn hits(&self, prefix: &str) -> usize {
            *self.hits.lock().expect("hits").get(prefix).unwrap_or(&0)
        }

        pub(crate) async fn serve(handler: fn(&str, &str) -> StubReply) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("stub bind");
            let addr = listener.local_addr().expect("stub addr");
            let hits: Arc<Mutex<std::collections::HashMap<String, usize>>> = Default::default();
            let hits_task = hits.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let hits = hits_task.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                            .await
                            .unwrap_or(0);
                        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                        let mut lines = raw.lines();
                        let request_line = lines.next().unwrap_or("");
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                        *hits
                            .lock()
                            .expect("hits")
                            .entry(path_key(&path))
                            .or_insert(0) += 1;
                        let reply = handler(&path, &body);
                        let response = format!(
                            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{}",
                            reply.status,
                            reason_phrase(reply.status),
                            reply.body.len(),
                            reply.body,
                        );
                        let _ =
                            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                                .await;
                        let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                    });
                }
            });
            StubServer {
                base_url: format!("http://{addr}"),
                hits,
            }
        }

        pub(crate) async fn serve_routes(cfg: FanoutStub) -> (String, StubHits) {
            // Single body-aware stub behind every fan-out test: DDG POSTs hit
            // `/html/` (body `s=` selects the continuation page), the
            // Startpage homepage GET hits `/`, search POSTs hit `/sp/search`.
            // Counts hits per route. std + tokio only.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("stub bind");
            let addr = listener.local_addr().expect("stub addr");
            let hits: StubHits = Default::default();
            let hits_task = hits.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (ddg_page1, ddg_page2, home, search, fail) = (
                        cfg.ddg_page1.clone(),
                        cfg.ddg_page2.clone(),
                        cfg.sp_home.clone(),
                        cfg.sp_search.clone(),
                        cfg.sp_search_fail,
                    );
                    let hits = hits_task.clone();
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
                        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                        *hits.lock().expect("hits").entry(route.clone()).or_insert(0) += 1;
                        let (status, reply) = if method == "POST" && route == "/html/" {
                            let page = if body.contains("s=") {
                                &ddg_page2
                            } else {
                                &ddg_page1
                            };
                            (200u16, page.clone())
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
                            "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{reply}",
                            reason_phrase(status),
                            reply.len(),
                        );
                        let _ =
                            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                                .await;
                        let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                    });
                }
            });
            (format!("http://{addr}"), hits)
        }

        pub(crate) async fn serve_slow(delay: std::time::Duration) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("stub bind");
            let addr = listener.local_addr().expect("stub addr");
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let body = "slow";
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{body}",
                            body.len(),
                        );
                        let _ =
                            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                                .await;
                        let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                    });
                }
            });
            StubServer {
                base_url: format!("http://{addr}"),
                hits: Default::default(),
            }
        }
    }

    pub(crate) fn path_key(path: &str) -> String {
        // Count by route prefix so query strings do not split counters.
        for known in ["/html/", "/sp/search", "/sp/", "/"] {
            if path == known || path.starts_with(known) {
                return known.to_string();
            }
        }
        path.to_string()
    }

    pub(crate) fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        }
    }

    pub(crate) type StubHits = Arc<Mutex<std::collections::HashMap<String, usize>>>;
    pub(crate) struct FanoutStub {
        ddg_page1: String,
        ddg_page2: String,
        sp_home: String,
        sp_search: String,
        sp_search_fail: bool,
    }
    impl FanoutStub {
        pub(crate) fn single_page(
            ddg: String,
            sp_home: String,
            sp_search: String,
            sp_search_fail: bool,
        ) -> Self {
            FanoutStub {
                ddg_page2: ddg.clone(),
                ddg_page1: ddg,
                sp_home,
                sp_search,
                sp_search_fail,
            }
        }
    }
    pub(crate) fn ddg_rows(url: &str, title: &str, snippet: &str) -> String {
        format!(
            "<html><body><div class=\"result\"><h2 class=\"result__title\"><a class=\"result__a\" href=\"{url}\">{title}</a></h2><div class=\"result__snippet\">{snippet}</div></div></body></html>"
        )
    }
    pub(crate) fn sp_rows(url: &str, title: &str, snippet: &str) -> String {
        format!(
            "<html><body><div class=\"result\"><a class=\"result-link\" href=\"{url}\"><h2 class=\"wgl-title\">{title}</h2></a><p class=\"description\">{snippet}</p></div></body></html>"
        )
    }
    pub(crate) fn sp_home_form() -> String {
        "<html><body><form action=\"/sp/search\" method=\"post\"><input type=\"hidden\" name=\"sc\" value=\"tok\"/></form></body></html>".to_string()
    }
}
