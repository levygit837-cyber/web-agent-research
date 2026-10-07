//! Shared `#[cfg(test)]` HTTP stubs for the search engines and the fan-out.

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
                        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    pub(crate) async fn serve_slow(delay: std::time::Duration) -> Self {
        Self::serve_slow_body(delay, "slow".to_string()).await
    }

    /// Replies 200 with `body` after `delay` (tokio time), counting every
    /// request under its path key.
    pub(crate) async fn serve_slow_body(delay: std::time::Duration, body: String) -> Self {
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
                    let path = raw
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    *hits
                        .lock()
                        .expect("hits")
                        .entry(path_key(&path))
                        .or_insert(0) += 1;
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ =
                        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                });
            }
        });
        StubServer {
            base_url: format!("http://{addr}"),
            hits,
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
                    // Every page carries DDG's `id="links"` results
                    // container (#104), so a zero-row page is a recognized
                    // SERP, not drift.
                    let body = if request_index < 4 {
                        let s = (request_index + 1) * 12;
                        format!(
                            r#"<html><body><div id="links"></div><form action="/html/" method="post"><input type="hidden" name="s" value="{s}"/><input type="hidden" name="vqd" value="tok"/><input type="hidden" name="q" value="q"/></form></body></html>"#
                        )
                    } else {
                        r#"<html><body><div id="links"></div></body></html>"#.to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ =
                        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
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
                        // Startpage's `w-gl` results container (#104).
                        r#"<html><body><div class="w-gl"></div></body></html>"#.to_string()
                    } else {
                        "<html><body><form action=\"/sp/search\" method=\"post\"><input type=\"hidden\" name=\"sc\" value=\"tok\"/></form></body></html>".to_string()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/html\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ =
                        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
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
    // `/search` covers Brave/Yahoo/Bing's single-GET shape (#66);
    // `/api/v1/crates` the crates.io API (#109); the others predate them.
    for known in [
        "/html/",
        "/sp/search",
        "/sp/",
        "/search",
        "/api/v1/crates",
        "/",
    ] {
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
