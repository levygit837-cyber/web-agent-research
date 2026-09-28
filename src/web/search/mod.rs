//! Fetch-only web search engine: DDG + Startpage over plain HTTP.
//!
//! Small Interface (`search_multi`, eleven pure helpers, three `#[doc(hidden)]`
//! base-URL overrides) over provider legs (`ddg`, `startpage`), merge
//! (`dedup`), fan-out (`fanout`) and codecs (`decode`). Callers cross only
//! this root; provider forms and deadlines stay inside.
//!
//! `ChainPosition`'s `sec-fetch-site` derivation ports Obscura's
//! `request_fetch_site` (Apache-2.0, h4ckf0r0day/obscura@542df14); see
//! `THIRD-PARTY-NOTICES.md`. `apply_navigation_headers`'s own header order
//! is independently verified against live Chrome, not ported (see its doc).

pub(crate) mod cache;
pub mod ddg;
pub mod decode;
pub mod dedup;
pub mod fanout;
pub mod startpage;
pub mod tool;
pub mod types;
#[doc(hidden)]
pub use ddg::ddg_search_with_base;
pub use ddg::{
    is_ddg_anomaly, parse_continuation_form, parse_ddg_html, unwrap_ddg_url, DDG_HTML_URL,
    DDG_REFERER,
};
pub use dedup::{dedup_key, merge_sources};
#[doc(hidden)]
pub use fanout::search_multi_with_bases;
pub use fanout::{all_failed_message, search_multi};
#[doc(hidden)]
pub use startpage::startpage_search_with_base;
pub use startpage::{
    is_startpage_challenge, parse_search_form_inputs, parse_startpage_html, sanitize_startpage_url,
    STARTPAGE_HOME_URL, STARTPAGE_SEARCH_URL,
};

/// Position of one HTTP call inside a request chain (#59: "one engine leg
/// plus its follow-ups"). Chrome's `sec-fetch-site` is `none` only for the
/// first request; a same-origin follow-up (DDG's `s`/`vqd` continuation
/// re-POST, the Startpage homepage -> search POST, or its direct-GET
/// fallback) sends `same-origin` (port of Obscura `request_fetch_site`,
/// `crates/obscura-net/src/client.rs:438-450`, Apache-2.0,
/// h4ckf0r0day/obscura@542df14; see `THIRD-PARTY-NOTICES.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainPosition {
    First,
    FollowUp,
}

impl ChainPosition {
    fn sec_fetch_site(self) -> &'static str {
        match self {
            ChainPosition::First => "none",
            ChainPosition::FollowUp => "same-origin",
        }
    }
}

/// Chrome navigation header order (#58): `sec-ch-ua*`,
/// `upgrade-insecure-requests`, `user-agent`, `accept`, `sec-fetch-*`,
/// `accept-encoding`, `accept-language`, then `referer`/`origin`/
/// `content-type` when the caller has them. Verified directly against a
/// live Chrome capture via `https://tls.peet.ws/api/all` (see the PR body)
/// and matches the issue's acceptance order.
///
/// This deliberately does NOT reuse Obscura's insertion order verbatim
/// (`crates/obscura-net/src/client.rs:1487-1498`, Apache-2.0,
/// h4ckf0r0day/obscura@542df14), which puts `referer` right after
/// `sec-fetch-dest`, before `accept-language`: Obscura never sets an
/// explicit `Accept-Encoding` and instead lets reqwest append it after
/// `accept-language` as a side effect of its own header-insertion quirk
/// (their comment on the same lines says as much). #57 requires an
/// explicit `Accept-Encoding`, which removes that quirk and lets this
/// function place `accept-encoding` where live Chrome actually sends it --
/// right after `sec-fetch-dest`, ahead of `referer`. Only the client-hints
/// GREASE algorithm (`chrome_client_hints`, `profile.rs`) and the
/// `sec-fetch-site` derivation (`ChainPosition`) are ported from Obscura.
/// `reqwest` still inserts its own `Host` after every header this
/// function sets -- on HTTP/1.1 that lands last on the wire regardless, and
/// there is no earlier seam to move it without the #56 transport.
pub(crate) fn apply_navigation_headers(
    builder: reqwest::RequestBuilder,
    profile: &crate::web::profile::BrowserProfile,
    position: ChainPosition,
    referer: Option<&str>,
) -> reqwest::RequestBuilder {
    let builder = builder
        .header("sec-ch-ua", &profile.sec_ch_ua)
        .header("sec-ch-ua-mobile", profile.sec_ch_ua_mobile)
        .header("sec-ch-ua-platform", profile.sec_ch_ua_platform)
        .header("upgrade-insecure-requests", "1")
        .header("user-agent", &profile.user_agent)
        .header("accept", profile.accept)
        .header("sec-fetch-site", position.sec_fetch_site())
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-user", "?1")
        .header("sec-fetch-dest", "document")
        .header("accept-encoding", profile.accept_encoding)
        .header("accept-language", profile.accept_language);
    match referer {
        Some(referer) => builder.header("Referer", referer),
        None => builder,
    }
}

/// `scheme://host[:port]` of `url`, for the `Origin` header on same-origin
/// form POSTs: Chrome sends `Origin` on POST/PUT/PATCH/DELETE even
/// same-origin (MDN "Origin header": "user agents add [it] to ... same-origin
/// requests except for GET or HEAD requests"). `None` on an unparseable URL
/// (never sent, rather than sending a wrong value).
pub(crate) fn request_origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

/// Per-request timeout ceiling: `LEG_TIMEOUT_SECS` in production, 1 s under
/// `cfg(test)` so the slow-stub leg-timeout test runs in milliseconds while
/// still exercising the real `.timeout(...)` path in `ddg_post` and friends.
pub(crate) fn leg_timeout() -> std::time::Duration {
    #[cfg(test)]
    return std::time::Duration::from_secs(1);
    #[cfg(not(test))]
    return std::time::Duration::from_secs(crate::web::search::types::LEG_TIMEOUT_SECS);
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};
    pub(crate) struct StubReply {
        status: u16,
        body: Vec<u8>,
        /// `Location` header value; set only by `redirect`.
        location: Option<String>,
    }

    impl StubReply {
        pub(crate) fn text(status: u16, body: &str) -> Self {
            StubReply {
                status,
                body: body.as_bytes().to_vec(),
                location: None,
            }
        }

        /// A 3xx reply with a `Location` header, for redirect-following tests.
        pub(crate) fn redirect(status: u16, location: &str) -> Self {
            StubReply {
                status,
                body: Vec::new(),
                location: Some(location.to_string()),
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
                        let location_header = reply
                            .location
                            .as_deref()
                            .map(|loc| format!("Location: {loc}\r\n"))
                            .unwrap_or_default();
                        let head = format!(
                            "HTTP/1.1 {} {}\r\n{location_header}Content-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n",
                            reply.status,
                            reason_phrase(reply.status),
                            reply.body.len(),
                        );
                        let mut wire = head.into_bytes();
                        wire.extend_from_slice(&reply.body);
                        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, &wire).await;
                        let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                    });
                }
            });
            StubServer {
                base_url: format!("http://{addr}"),
                hits,
            }
        }

        /// Always replies 200 with `body` verbatim and a matching
        /// `Content-Encoding` header (#57 gzip/brotli decode fixture test).
        pub(crate) async fn serve_encoded(encoding: &'static str, body: Vec<u8>) -> Self {
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
                    let body = body.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                            .await
                            .unwrap_or(0);
                        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                        let request_line = raw.lines().next().unwrap_or("");
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        *hits
                            .lock()
                            .expect("hits")
                            .entry(path_key(&path))
                            .or_insert(0) += 1;
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Encoding: {encoding}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n",
                            body.len(),
                        );
                        let mut wire = head.into_bytes();
                        wire.extend_from_slice(&body);
                        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, &wire).await;
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

        /// Records raw request header *lines* (verbatim, wire order) per
        /// request, for the #58 header-order hermetic test. Replies with a
        /// DDG continuation form (`s`+`vqd`) on the first 4 requests, each
        /// with a distinct `s` value so `ddg_search`'s "`s` stopped
        /// advancing" guard never trips early, then an empty page on the
        /// 5th: the chain always makes exactly 5 requests, so a regression
        /// that re-picks the profile per request would need to draw the
        /// same one of 9 profiles 4 times running by chance -- (1/9)^4, far
        /// below any plausible flaky-test threshold -- rather than the 1-in-9
        /// chance a 2-request chain would give it.
        pub(crate) async fn serve_capturing_headers() -> (Self, HeaderCapture) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("stub bind");
            let addr = listener.local_addr().expect("stub addr");
            let hits: Arc<Mutex<std::collections::HashMap<String, usize>>> = Default::default();
            let hits_task = hits.clone();
            let captured: HeaderCapture = Default::default();
            let captured_task = captured.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let hits = hits_task.clone();
                    let captured = captured_task.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                            .await
                            .unwrap_or(0);
                        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                        let mut lines = raw.split("\r\n");
                        let request_line = lines.next().unwrap_or("");
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        let header_lines: Vec<String> = lines
                            .take_while(|line| !line.is_empty())
                            .map(|line| line.to_string())
                            .collect();
                        let request_index = {
                            let mut hits = hits.lock().expect("hits");
                            let count = hits.entry(path_key(&path)).or_insert(0);
                            let index = *count;
                            *count += 1;
                            index
                        };
                        captured.lock().expect("captured").push(header_lines);
                        let body = if request_index < 4 {
                            let s = (request_index + 1) * 12;
                            format!(
                                r#"<html><body><form action="/html/" method="post"><input type="hidden" name="s" value="{s}"/><input type="hidden" name="vqd" value="tok"/><input type="hidden" name="q" value="q"/></form></body></html>"#
                            )
                        } else {
                            "<html><body></body></html>".to_string()
                        };
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
            (
                StubServer {
                    base_url: format!("http://{addr}"),
                    hits,
                },
                captured,
            )
        }

        /// Records raw request header *lines* per request for the Startpage
        /// chain (#58/#59 hermetic coverage): homepage GET on `/` replies with
        /// the `sc`-token form, then the search POST on `/sp/search` replies
        /// with an empty result page. Two requests total.
        pub(crate) async fn serve_capturing_headers_startpage() -> (Self, HeaderCapture) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("stub bind");
            let addr = listener.local_addr().expect("stub addr");
            let hits: Arc<Mutex<std::collections::HashMap<String, usize>>> = Default::default();
            let captured: HeaderCapture = Default::default();
            let captured_task = captured.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let captured = captured_task.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                            .await
                            .unwrap_or(0);
                        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                        let mut lines = raw.split("\r\n");
                        let request_line = lines.next().unwrap_or("");
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        let header_lines: Vec<String> = lines
                            .take_while(|line| !line.is_empty())
                            .map(|line| line.to_string())
                            .collect();
                        captured.lock().expect("captured").push(header_lines);
                        let route = path.split('?').next().unwrap_or("/");
                        let body = if route == "/sp/search" {
                            "<html><body></body></html>".to_string()
                        } else {
                            "<html><body><form action=\"/sp/search\" method=\"post\"><input type=\"hidden\" name=\"sc\" value=\"tok\"/></form></body></html>".to_string()
                        };
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
            (
                StubServer {
                    base_url: format!("http://{addr}"),
                    hits,
                },
                captured,
            )
        }
    }

    /// One entry per request received by `serve_capturing_headers`: raw
    /// header *name: value* lines in the exact order they arrived on the
    /// wire (header-name case as sent, never normalized).
    pub(crate) type HeaderCapture = Arc<Mutex<Vec<Vec<String>>>>;

    /// Lowercase header *names* only (order preserved, values dropped): the
    /// shape the #58 hermetic test compares against a profile's expected
    /// order.
    pub(crate) fn header_names(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|line| line.split_once(':'))
            .map(|(name, _)| name.trim().to_ascii_lowercase())
            .collect()
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
