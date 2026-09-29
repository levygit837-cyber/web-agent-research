//! Yahoo HTML leg of the fetch-only web search engine.
//!
//! Plain GET with the `YBV` cookie-chain hop sequence replayed in a
//! per-leg cookie jar (this leg's own dedicated client, redirects
//! disabled, so every hop's `Set-Cookie` is visible and replayed on the
//! next hop -- the shared client's default auto-follow policy would
//! silently drop them). DOM-parsed (`scraper`) into raw hits. Every
//! organic href is wrapped through `r.search.yahoo.com/.../RU=<url>/RS=`;
//! unwrapped here.
//!
//! Request shape and the YBV hop sequence are measured and
//! fixture-backed in `docs/research/search-engines.md` ("Yahoo HTML --
//! KEEP") and its `fixtures/yahoo/`. The consent-wall marker is
//! documented public research (not live-observed this session -- see its
//! doc comment). No code is ported from SearXNG's own `yahoo.py`
//! (AGPL-3.0, idea only -- see `THIRD-PARTY-NOTICES.md`).

use std::collections::HashMap;

use scraper::{Html, Selector};

use super::fanout::{map_transport_error, parse_retry_after};
use crate::web::profile::{pick_profile, BrowserProfile};
use crate::web::search::decode::{collapse_whitespace, percent_decode};
use crate::web::search::types::{
    Recency, SearchProvider, SearchProviderError, SearchResult, MAX_NUM_RESULTS,
};
use crate::web::search::{apply_navigation_headers, leg_timeout, ChainPosition};

/// Yahoo HTML search endpoint (#66). Tests override via
/// `yahoo_search_with_base`.
pub const YAHOO_SEARCH_URL: &str = "https://search.yahoo.com/search";

/// Maximum redirect hops this leg follows manually (measured: tracking
/// pixel -> gif -> back to `/search`, 3 real hops on a cold session; one
/// spare hop for a possible geo-domain redirect, matching the shape
/// SearXNG's own `yahoo.py` documents as `_YBV_HOPS = 4` -- idea only, not
/// copied).
const MAX_HOPS: usize = 4;

/// Yahoo consent-wall marker (documented via public Yahoo-scraping
/// research, not live-observed in the #65 evaluation session -- that
/// session saw zero blocks across 10 requests). From some IPs Yahoo
/// serves a `guce.yahoo.com`/`consent.yahoo.com` interstitial instead of
/// results: a `200 OK` with zero organic rows, functionally identical to
/// a bot wall for this leg's purpose even though it is nominally a cookie-
/// consent screen, not an anti-scraping challenge.
pub fn is_yahoo_challenge(final_url: &str) -> bool {
    final_url.contains("guce.yahoo.com") || final_url.contains("consent.yahoo.com")
}

/// Map a finished Yahoo HTTP exchange to rows or a typed leg error:
/// `is_yahoo_challenge(final_url)` -> `Challenge` (429); non-2xx without
/// that marker -> `Upstream` (503, carrying the real `status` and any
/// parsed `retry_after_secs` for #62's suspension mapping -- isolated
/// Yahoo HTTP 500s are a known, non-bot-wall occurrence per #73's live
/// measurement). Empty with no marker is `Ok(vec![])`.
pub(crate) fn map_yahoo_response(
    status: u16,
    body: &str,
    final_url: &str,
    query: &str,
    retry_after_secs: Option<u64>,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    if is_yahoo_challenge(final_url) {
        return Err(SearchProviderError::Challenge {
            provider: SearchProvider::Yahoo,
            detail: "Yahoo redirected to its consent/guce wall instead of search results. Retry via an accredited provider or later.".to_string(),
        });
    }
    if !(200..300).contains(&status) {
        return Err(SearchProviderError::Upstream {
            provider: SearchProvider::Yahoo,
            detail: format!("Yahoo HTML error ({status})"),
            status: Some(status),
            retry_after_secs,
        });
    }
    Ok(parse_yahoo_html(body, query))
}

/// Extract `name=value` pairs from every `Set-Cookie` response header,
/// keeping only the cookie-pair segment (before the first `;` attribute
/// like `Path=`/`Secure`/`Max-Age=`).
fn extract_set_cookies(response: &reqwest::Response) -> Vec<(String, String)> {
    response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|raw| {
            let pair = raw.split(';').next().unwrap_or("").trim();
            let (name, value) = pair.split_once('=')?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// Serialize a per-leg cookie jar as one `Cookie` header value.
fn cookie_header(jar: &HashMap<String, String>) -> String {
    jar.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// One Yahoo leg: manual redirect-follow loop, up to [`MAX_HOPS`],
/// replaying `Set-Cookie` values from each hop as the `Cookie` header on
/// the next (the `YBV` chain: hop 1 sets a 60s tracking cookie, hop 2 sets
/// the real 24h cache cookie and redirects back, hop 3 returns the SERP).
/// Uses a dedicated leg-local client with redirects disabled so every
/// hop's headers stay visible -- the shared search client's default
/// auto-follow policy would resolve the whole chain internally and drop
/// every intermediate `Set-Cookie` (no cookie-store feature is enabled on
/// it, by design: that jar would otherwise leak across engines/legs).
/// Stops early on the first `200`, on a non-redirect status, or on a
/// redirect with no `Location` header. Returns status, body, final URL,
/// and any parsed `Retry-After` header (#62); transport errors map to
/// `Timeout` (504, `is_timeout`) or `Upstream` (503).
async fn yahoo_hop_chain(
    query: &str,
    url: &str,
    profile: &BrowserProfile,
) -> Result<(u16, String, String, Option<u64>), SearchProviderError> {
    let leg_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| {
            map_transport_error(
                SearchProvider::Yahoo,
                query,
                false,
                format!("Yahoo leg client build failed: {err}"),
            )
        })?;
    let mut jar: HashMap<String, String> = HashMap::new();
    let mut current = url.to_string();
    let mut position = ChainPosition::First;
    for _ in 0..MAX_HOPS {
        let mut builder =
            apply_navigation_headers(leg_client.get(&current), profile, position, None)
                .timeout(leg_timeout());
        if !jar.is_empty() {
            builder = builder.header("Cookie", cookie_header(&jar));
        }
        let response = builder.send().await.map_err(|err| {
            map_transport_error(
                SearchProvider::Yahoo,
                query,
                err.is_timeout(),
                format!("Yahoo request failed: {err}"),
            )
        })?;
        let status = response.status().as_u16();
        let retry_after = parse_retry_after(&response);
        for (name, value) in extract_set_cookies(&response) {
            jar.insert(name, value);
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let final_url_this_hop = current.clone();
        let text = response.text().await.map_err(|err| {
            map_transport_error(
                SearchProvider::Yahoo,
                query,
                err.is_timeout(),
                format!("Yahoo body read failed: {err}"),
            )
        })?;
        if !(300..400).contains(&status) {
            return Ok((status, text, final_url_this_hop, retry_after));
        }
        let Some(location) = location else {
            // A redirect status with no Location header: nothing more to
            // follow, surface it as-is.
            return Ok((status, text, final_url_this_hop, retry_after));
        };
        let Ok(base) = url::Url::parse(&current) else {
            return Ok((status, text, final_url_this_hop, retry_after));
        };
        let Ok(next) = base.join(&location) else {
            return Ok((status, text, final_url_this_hop, retry_after));
        };
        let next_str = next.to_string();
        if is_yahoo_challenge(&next_str) {
            // The consent/guce wall is an external Yahoo host this leg
            // never needs to actually fetch: the redirect target alone is
            // the challenge signal (`map_yahoo_response` checks
            // `is_yahoo_challenge(final_url)` before status), so stop here
            // rather than issuing a real request to it.
            return Ok((status, text, next_str, retry_after));
        }
        current = next_str;
        // Every hop after the first is same-origin follow-up traffic
        // inside one chain, matching every other engine's `ChainPosition`
        // convention (#59).
        position = ChainPosition::FollowUp;
    }
    // Hop budget exhausted without a terminal status: surface the last
    // known state as Upstream via a final direct request result -- in
    // practice unreachable at MAX_HOPS=4 given the measured 3-hop chain,
    // but keeps the loop bounded rather than looping forever on a
    // pathological redirect cycle.
    Err(SearchProviderError::Upstream {
        provider: SearchProvider::Yahoo,
        detail: format!("Yahoo redirect chain exceeded {MAX_HOPS} hops"),
        status: None,
        retry_after_secs: None,
    })
}

/// Yahoo leg: `p=<query>` GET through the YBV hop chain, page 1 only (#66
/// human decision; `b`/`pz` pagination is #73's job).
pub(crate) async fn yahoo_search(
    query: &str,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    // One profile per leg (#59): every hop of this chain reuses it.
    let profile = pick_profile();
    let url = format!(
        "{base}?p={}",
        crate::web::search::decode::percent_encode(query)
    );
    let (status, body, final_url, retry_after) = yahoo_hop_chain(query, &url, &profile).await?;
    map_yahoo_response(status, &body, &final_url, query, retry_after)
}

/// Unwrap a Yahoo result href (measured shape:
/// `r.search.yahoo.com/_ylt=.../RU=<url-encoded target>/RS=...`): take
/// everything from the first `http` after `/RU=` up to the nearest `/RS`
/// or `/RK`, then percent-decode. A non-wrapped absolute http(s) href
/// passes through as-is. Anything else returns `None` (row skipped).
pub fn unwrap_yahoo_url(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Some(ru_pos) = trimmed.find("/RU=") else {
        // A `r.search.yahoo.com` href with no `/RU=` segment is a
        // malformed/incomplete wrapper, not a real destination -- skip it
        // rather than treating the tracker URL itself as the target.
        // Anything else that is a genuine absolute http(s) URL (never
        // wrapped in the first place) passes through as-is.
        if trimmed.contains("r.search.yahoo.com") {
            return None;
        }
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Some(trimmed.to_string());
        }
        return None;
    };
    let after_ru = &trimmed[ru_pos + 1..];
    let http_offset = after_ru.find("http")?;
    let target_start = ru_pos + 1 + http_offset;
    let rest = &trimmed[target_start..];
    let end = ["/RS", "/RK"]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
    let encoded = &rest[..end];
    let decoded = percent_decode(encoded);
    if decoded.starts_with("http://") || decoded.starts_with("https://") {
        Some(decoded)
    } else {
        None
    }
}

/// Parse Yahoo HTML (DOM via `scraper` -- measured shape: `div.algo-sr`
/// rows, title in `.compTitle a h3`, snippet in `.compText p`).
pub fn parse_yahoo_html(html: &str, query: &str) -> Vec<SearchResult> {
    let document = Html::parse_document(html);
    let row_sel = Selector::parse("div.algo-sr").expect("static row selector");
    let link_sel = Selector::parse(".compTitle a").expect("static link selector");
    let title_sel = Selector::parse("h3").expect("static title selector");
    let snippet_sel = Selector::parse(".compText p").expect("static snippet selector");
    let mut rows = Vec::new();
    for row in document.select(&row_sel) {
        let Some(link) = row.select(&link_sel).next() else {
            continue;
        };
        let href = link.value().attr("href").unwrap_or("");
        let Some(url) = unwrap_yahoo_url(href) else {
            continue;
        };
        let title = link
            .select(&title_sel)
            .next()
            .map(|t| collapse_whitespace(&t.text().collect::<String>()))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let snippet = row
            .select(&snippet_sel)
            .next()
            .map(|s| collapse_whitespace(&s.text().collect::<String>()))
            .unwrap_or_default();
        if rows.len() >= MAX_NUM_RESULTS {
            continue;
        }
        rows.push(SearchResult {
            title,
            url,
            snippet,
            provider: SearchProvider::Yahoo,
            query: query.to_string(),
            rank: rows.len(),
            published_date: None,
        });
    }
    rows
}

#[doc(hidden)]
pub async fn yahoo_search_with_base(
    _client: &reqwest::Client,
    query: &str,
    _recency: Option<Recency>,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    // Yahoo has no documented recency filter reachable on page 1 without
    // extra params (#66 scope: page 1 only); accepted for signature
    // symmetry, ignored. `_client` is likewise accepted for signature
    // symmetry with every other engine leg but unused: this leg always
    // builds its own dedicated no-redirect client (see `yahoo_hop_chain`).
    yahoo_search(query, base).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::test_support::{StubReply, StubServer};

    /// Real fixture-backed parse (docs/research/search-engines/fixtures/
    /// yahoo/Q1-success.html): 4 organic rows, `r.search.yahoo.com/.../RU=`
    /// wrappers unwrapped to their direct targets.
    #[test]
    fn yahoo_parse_real_fixture_unwraps_redirects() {
        let html =
            include_str!("../../../docs/research/search-engines/fixtures/yahoo/Q1-success.html");
        let rows = parse_yahoo_html(html, "tokio rust crate documentation");
        assert_eq!(
            rows.len(),
            4,
            "fixture holds 4 organic div.algo-sr rows: {rows:?}"
        );
        assert_eq!(rows[0].url, "https://docs.rs/tokio/latest/tokio/");
        assert_eq!(rows[0].title, "tokio - Rust - Docs.rs");
        assert!(rows[0].snippet.contains("Tokio is an event-driven"));
        assert_eq!(rows[0].provider, SearchProvider::Yahoo);
        assert_eq!(rows[1].url, "https://hid-io.github.io/tokio/");
        assert_eq!(rows[1].title, "tokio - Rust");
        assert_eq!(rows[2].url, "https://tokio.rs/");
        assert_eq!(
            rows[3].url,
            "https://generalistprogrammer.com/tutorials/tokio-rust-crate-guide"
        );
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.rank, i);
        }
    }

    #[test]
    fn yahoo_unwrap_direct_urls() {
        // Real wrapped href, taken verbatim from the fixture.
        assert_eq!(
            unwrap_yahoo_url(
                "https://r.search.yahoo.com/_ylt=A2RRhkCUjrpqkbULHwBXNyoA;_ylu=Y29sbwN1cy1lYXN0LTEEcG9zAzEEdnRpZAMEc2VjA3Ny/RV=2/RE=1791820693/RO=10/RU=https%3a%2f%2fdocs.rs%2ftokio%2flatest%2ftokio%2f/RK=2/RS=VQgICTvUGSnScH0jtS.i4GS21eU-"
            ),
            Some("https://docs.rs/tokio/latest/tokio/".to_string())
        );
        assert_eq!(
            unwrap_yahoo_url(
                "https://r.search.yahoo.com/_ylt=x/RU=https%3a%2f%2ftokio.rs%2f/RS=abc-"
            ),
            Some("https://tokio.rs/".to_string())
        );
        // Non-wrapped absolute URL passes through.
        assert_eq!(
            unwrap_yahoo_url("https://example.com/direct"),
            Some("https://example.com/direct".to_string())
        );
        // No /RU= segment, relative, or javascript: all skip.
        assert_eq!(unwrap_yahoo_url("https://r.search.yahoo.com/_ylt=x"), None);
        assert_eq!(unwrap_yahoo_url("/relative/path"), None);
        assert_eq!(unwrap_yahoo_url("javascript:void(0)"), None);
        assert_eq!(unwrap_yahoo_url(""), None);
    }

    /// Regression: a consent/guce redirect target must map to `Challenge`
    /// even though it is a plain 200-ish flow, not a themed block body --
    /// documented public research (see the module doc), not a live #65
    /// fixture. Shows the red run: without the `is_yahoo_challenge` final-
    /// URL check, this would fall through to `Ok(parse_yahoo_html(...))`,
    /// silently returning zero rows as a normal (if empty) success instead
    /// of a typed `Challenge`.
    #[test]
    fn yahoo_consent_wall_maps_challenge() {
        let err = map_yahoo_response(
            200,
            "<html>no results</html>",
            "https://guce.yahoo.com/consent?sessionId=abc",
            "q",
            None,
        )
        .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
    }

    #[test]
    fn yahoo_500_maps_upstream_503_not_challenge() {
        // #73's live measurement: isolated Yahoo HTTP 500s are a known
        // non-bot-wall occurrence; must map Upstream, never Challenge.
        let err = map_yahoo_response(
            500,
            "boom",
            "https://search.yahoo.com/search?p=q",
            "q",
            None,
        )
        .unwrap_err();
        assert_eq!(err.http_status(), 503);
        assert_eq!(err.code(), "upstream");
    }

    #[tokio::test]
    async fn yahoo_ybv_hop_chain_replays_cookie_and_lands_on_serp() {
        // 3-hop chain matching the measured shape: hop1 sets YBV=v0.1 and
        // redirects to the pixel; hop2 (must have received v0.1) sets the
        // real YBV=v0.2 and redirects back; hop3 (must have received
        // v0.2) returns the SERP. Any hop not receiving the expected prior
        // cookie fails the assertion instead of silently proceeding.
        let stub = StubServer::serve(|path: &str, _: &str| {
            if path.starts_with("/search") {
                StubReply::redirect(307, "/_bv/v.gif")
            } else if path.starts_with("/_bv/v.gif") {
                StubReply::redirect(307, "/search?p=q")
            } else {
                unreachable!("unexpected path {path}")
            }
        })
        .await;
        // The generic `serve` stub can't set Set-Cookie or assert on
        // Cookie headers per-hop; use the header-capturing variant, whose
        // handler always answers 200 -- so cover the cookie ROUND-TRIP
        // (jar receives and replays) using `serve_capturing_headers`
        // instead, which is designed for exactly this: it never sets
        // cookies, so this test instead directly exercises the loop via
        // a purpose-built 3-response StubServer below.
        drop(stub);

        // Purpose-built stub: 3 sequential responses on the same route,
        // each setting a distinct Set-Cookie and asserting the previous
        // hop's cookie was received.
        use std::sync::{Arc, Mutex};
        let hop = Arc::new(Mutex::new(0usize));
        let hop_task = hop.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let hop_task = hop_task.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                        .await
                        .unwrap_or(0);
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    let has_cookie_v01 = raw.contains("YBV=v0.1");
                    let has_cookie_v02 = raw.contains("YBV=v0.2");
                    // Scoped block: the `MutexGuard` must drop before the
                    // next `.await` below, or the enclosing future is not
                    // `Send` (a std `MutexGuard` never is) and `tokio::spawn`
                    // rejects it at compile time.
                    let this_hop = {
                        let mut hop_count = hop_task.lock().expect("hop");
                        let this_hop = *hop_count;
                        *hop_count += 1;
                        this_hop
                    };
                    let response = match this_hop {
                        0 => {
                            "HTTP/1.1 307 Temporary Redirect\r\nSet-Cookie: YBV=v0.1.tracking\r\nLocation: /_bv/v.gif\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                        }
                        1 => {
                            assert!(has_cookie_v01, "hop 2 must receive the v0.1 cookie from hop 1");
                            "HTTP/1.1 307 Temporary Redirect\r\nSet-Cookie: YBV=v0.2.cache\r\nLocation: /search?p=q\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                        }
                        _ => {
                            assert!(has_cookie_v02, "hop 3 must receive the v0.2 cookie from hop 2");
                            let body = "<div class=\"algo-sr\"></div>";
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        }
                    };
                    let _ =
                        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                });
            }
        });
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();
        let rows = yahoo_search_with_base(&client, "q", None, &format!("{base}/search"))
            .await
            .expect("3-hop chain must succeed");
        assert_eq!(rows.len(), 0, "empty result page, but must not error");
        assert_eq!(*hop.lock().expect("hop"), 3, "must make exactly 3 requests");
    }

    #[tokio::test]
    async fn yahoo_challenge_maps_from_final_redirect_target() {
        let stub = StubServer::serve(|_: &str, _: &str| {
            StubReply::redirect(302, "https://guce.yahoo.com/consent?sessionId=x")
        })
        .await;
        let client = reqwest::Client::new();
        let err = yahoo_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
    }

    #[tokio::test]
    async fn yahoo_500_leg_costs_one_request() {
        let stub = StubServer::serve(|_: &str, _: &str| StubReply::text(500, "boom")).await;
        let client = reqwest::Client::new();
        let err = yahoo_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .unwrap_err();
        assert_eq!(err.http_status(), 503);
        assert_eq!(stub.hits("/search"), 1);
    }

    #[tokio::test]
    async fn yahoo_slow_stub_times_out() {
        let stub = StubServer::serve_slow(std::time::Duration::from_secs(5)).await;
        let client = reqwest::Client::new();
        let err = yahoo_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .expect_err("slow stub must time out at leg level");
        assert_eq!(err.http_status(), 504);
    }

    #[test]
    fn yahoo_clamp_single_page() {
        let mut html = String::new();
        for i in 0..25 {
            html.push_str(&format!(
                r#"<div class="algo-sr"><div class="compTitle"><a href="https://example.com/s{i}"><h3>T{i}</h3></a></div><div class="compText"><p>d{i}</p></div></div>"#
            ));
        }
        let rows = parse_yahoo_html(&html, "q");
        assert_eq!(rows.len(), 20);
    }
}
