//! Bing HTML leg of the fetch-only web search engine.
//!
//! Plain GET with an explicit `mkt`/`setlang` market (mandatory: Bing
//! silently geo/language-localizes on client IP with zero explicit signal
//! otherwise), DOM-parsed (`scraper`) into raw hits. Every organic href is
//! wrapped through `bing.com/ck/a?...&u=a1<base64url>`; unwrapped here.
//!
//! Request shape and the `mkt` finding are measured and fixture-backed in
//! `docs/research/search-engines.md` ("Bing HTML -- KEEP, with a mandatory
//! param") and its `fixtures/bing/`. The `challenge/verify` block marker is
//! documented public research (not live-observed this session -- see its
//! doc comment). No code is ported from SearXNG's own `bing.py` (AGPL-3.0,
//! idea only -- see `THIRD-PARTY-NOTICES.md`).

use scraper::{Html, Selector};

use super::fanout::{map_transport_error, parse_retry_after};
use crate::web::profile::{pick_profile, BrowserProfile};
use crate::web::search::decode::{base64url_decode, collapse_whitespace};
use crate::web::search::types::{
    Recency, SearchProvider, SearchProviderError, SearchResult, BING_MARKET_DEFAULT,
    MAX_NUM_RESULTS,
};
use crate::web::search::{apply_navigation_headers, leg_timeout, ChainPosition};

/// Bing HTML search endpoint (#66). Tests override via `bing_search_with_base`.
pub const BING_SEARCH_URL: &str = "https://www.bing.com/search";

/// `SEARCH_BING_MARKET` (`mkt` value, e.g. `en-US`); default
/// [`BING_MARKET_DEFAULT`]. Mandatory on every request per the #65
/// evaluation: with no explicit `mkt`, Bing auto-localized to `pt-BR` from
/// a Brazilian residential IP and returned 0/10 relevant results (Tokio
/// Marine insurance instead of the Rust crate); `mkt=en-US` alone fixed it
/// to 5/5. An invalid (empty) override falls back to the default rather
/// than sending a blank `mkt`.
pub fn bing_market() -> String {
    let raw = std::env::var("SEARCH_BING_MARKET").unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        BING_MARKET_DEFAULT.to_string()
    } else {
        trimmed.to_string()
    }
}

/// `setlang` derived from a `mkt` value's language subtag (e.g. `en-US` ->
/// `en`, `pt-BR` -> `pt`). A market with no `-` (already bare) is returned
/// lowercased as-is.
pub fn setlang_from_market(mkt: &str) -> String {
    mkt.split('-').next().unwrap_or(mkt).to_ascii_lowercase()
}

/// Bing block-page marker (documented via public Bing-scraping research,
/// not live-observed in the #65 evaluation session -- that session saw
/// zero blocks across 11 requests). Bing's anti-bot escalates to a
/// same-status-200 CAPTCHA gate wired through a `challenge/verify`
/// endpoint reference embedded in the page's own JS (`"verifyEndpoint":
/// "https://www.bing.com/challenge/verify?..."`, `captchaSuccessPostMessage`)
/// rather than a themed block body -- so this checks the JS marker, never a
/// status code alone (Bing serves the challenge with `200`, same as a real
/// SERP).
pub fn is_bing_challenge(body: &str) -> bool {
    body.contains("challenge/verify") || body.contains("captchaSuccessPostMessage")
}

/// Map a finished Bing HTTP exchange to rows or a typed leg error:
/// `is_bing_challenge` marker -> `Challenge` (429) even on status 200 (the
/// documented pattern: Bing's gate answers 200); HTTP `429` also maps
/// `Challenge` for #62 parity with every other engine leg; other non-2xx ->
/// `Upstream` (503, carrying the real `status` and any parsed
/// `retry_after_secs`). Empty with no marker is `Ok(vec![])`.
pub(crate) fn map_bing_response(
    status: u16,
    body: &str,
    query: &str,
    retry_after_secs: Option<u64>,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    if status == 429 || is_bing_challenge(body) {
        return Err(SearchProviderError::Challenge {
            provider: SearchProvider::Bing,
            detail: "Bing blocked the request with a bot-detection challenge (challenge/verify gate or HTTP 429). Retry via an accredited provider or later.".to_string(),
        });
    }
    if !(200..300).contains(&status) {
        return Err(SearchProviderError::Upstream {
            provider: SearchProvider::Bing,
            detail: format!("Bing HTML error ({status})"),
            status: Some(status),
            retry_after_secs,
        });
    }
    Ok(parse_bing_html(body, query))
}

/// One Bing GET: `q`/`mkt`/`setlang` query params (mkt/setlang always
/// present, never optional), desktop navigation headers, per-request
/// timeout ceiling. Returns status + body + any parsed `Retry-After`
/// header (#62); transport errors map to `Timeout` (504, `is_timeout`) or
/// `Upstream` (503).
async fn bing_get(
    client: &reqwest::Client,
    url: &str,
    query: &str,
    mkt: &str,
    setlang: &str,
    profile: &BrowserProfile,
) -> Result<(u16, String, Option<u64>), SearchProviderError> {
    let response = apply_navigation_headers(client.get(url), profile, ChainPosition::First, None)
        .query(&[("q", query), ("mkt", mkt), ("setlang", setlang)])
        .timeout(leg_timeout())
        .send()
        .await
        .map_err(|err| {
            map_transport_error(
                SearchProvider::Bing,
                query,
                err.is_timeout(),
                format!("Bing request failed: {err}"),
            )
        })?;
    let status = response.status().as_u16();
    let retry_after = parse_retry_after(&response);
    let text = response.text().await.map_err(|err| {
        map_transport_error(
            SearchProvider::Bing,
            query,
            err.is_timeout(),
            format!("Bing body read failed: {err}"),
        )
    })?;
    Ok((status, text, retry_after))
}

/// Bing leg: single GET, page 1 only (#66 human decision -- Bing cannot
/// paginate `first=N` without JS at all, confirmed live by #73's
/// measurement: pages 1-4 byte-identical). `mkt`/`setlang` always sent,
/// from [`bing_market`]/[`setlang_from_market`].
pub(crate) async fn bing_search(
    client: &reqwest::Client,
    query: &str,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    let profile = pick_profile();
    let mkt = bing_market();
    let setlang = setlang_from_market(&mkt);
    let (status, body, retry_after) =
        bing_get(client, base, query, &mkt, &setlang, &profile).await?;
    map_bing_response(status, &body, query, retry_after)
}

/// Unwrap a Bing organic href (measured shape:
/// `bing.com/ck/a?...&u=a1<base64url-no-padding>`; distinct from DDG's
/// `uddg=` percent-encoding scheme). Non-wrapped absolute http(s) hrefs
/// pass through as-is (a future Bing markup change might stop wrapping).
/// Anything else (relative, javascript:, missing `u=a1` segment,
/// undecodable base64) returns `None` (row skipped).
pub fn unwrap_bing_url(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.contains("bing.com/ck/a?") {
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Some(trimmed.to_string());
        }
        return None;
    }
    let qpos = trimmed.find('?')?;
    for pair in trimmed[qpos + 1..].split('&') {
        let (key, value) = match pair.find('=') {
            Some(eq) => (&pair[..eq], &pair[eq + 1..]),
            None => continue,
        };
        if key != "u" {
            continue;
        }
        let rest = value.strip_prefix("a1")?;
        let decoded = base64url_decode(rest)?;
        if decoded.starts_with("http://") || decoded.starts_with("https://") {
            return Some(decoded);
        }
        return None;
    }
    None
}

/// Parse Bing HTML (DOM via `scraper` -- measured shape: `li.b_algo` rows
/// inside `ol#b_results`, title in `h2 a`, snippet in `div.b_caption p`).
pub fn parse_bing_html(html: &str, query: &str) -> Vec<SearchResult> {
    let document = Html::parse_document(html);
    let row_sel = Selector::parse("li.b_algo").expect("static row selector");
    let title_link_sel = Selector::parse("h2 a").expect("static title link selector");
    let snippet_sel = Selector::parse("div.b_caption p").expect("static snippet selector");
    let mut rows = Vec::new();
    for row in document.select(&row_sel) {
        let Some(link) = row.select(&title_link_sel).next() else {
            continue;
        };
        let href = link.value().attr("href").unwrap_or("");
        let Some(url) = unwrap_bing_url(href) else {
            continue;
        };
        let title = collapse_whitespace(&link.text().collect::<String>());
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
            provider: SearchProvider::Bing,
            query: query.to_string(),
            rank: rows.len(),
            published_date: None,
        });
    }
    rows
}

#[doc(hidden)]
pub async fn bing_search_with_base(
    client: &reqwest::Client,
    query: &str,
    _recency: Option<Recency>,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    // Bing HTML has no documented recency filter reachable without JS
    // (#66 scope: page 1 only); accepted for signature symmetry, ignored.
    bing_search(client, query, base).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::test_support::{StubReply, StubServer};

    /// Real fixture-backed parse (docs/research/search-engines/fixtures/
    /// bing/Q1-success-mktUS.html): 4 organic rows, `ck/a?...u=a1...`
    /// wrappers unwrapped to their direct targets.
    #[test]
    fn bing_parse_real_fixture_unwraps_redirects() {
        let html = include_str!(
            "../../../docs/research/search-engines/fixtures/bing/Q1-success-mktUS.html"
        );
        let rows = parse_bing_html(html, "tokio rust crate documentation");
        assert_eq!(
            rows.len(),
            4,
            "fixture holds 4 organic li.b_algo rows: {rows:?}"
        );
        assert_eq!(rows[0].url, "https://tokio.rs/");
        assert!(rows[0].title.contains("Tokio"));
        assert!(rows[0].snippet.contains("Tokio is a runtime"));
        assert_eq!(rows[0].provider, SearchProvider::Bing);
        assert_eq!(rows[1].url, "https://hid-io.github.io/tokio/");
        assert_eq!(
            rows[2].url,
            "https://generalistprogrammer.com/tutorials/tokio-rust-crate-guide"
        );
        assert_eq!(rows[3].url, "https://docs.rs/crate/tokio/latest");
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.rank, i);
        }
    }

    #[test]
    fn bing_unwrap_direct_urls() {
        // Real wrapped hrefs, taken verbatim from the fixture.
        assert_eq!(
            unwrap_bing_url(
                "https://www.bing.com/ck/a?!&&p=925329f2&ptn=3&ver=2&hsh=4&fclid=x&u=a1aHR0cHM6Ly90b2tpby5ycy8&ntb=1"
            ),
            Some("https://tokio.rs/".to_string())
        );
        assert_eq!(
            unwrap_bing_url(
                "https://www.bing.com/ck/a?u=a1aHR0cHM6Ly9kb2NzLnJzL2NyYXRlL3Rva2lvL2xhdGVzdA"
            ),
            Some("https://docs.rs/crate/tokio/latest".to_string())
        );
        // Non-wrapped absolute URL passes through.
        assert_eq!(
            unwrap_bing_url("https://example.com/direct"),
            Some("https://example.com/direct".to_string())
        );
        // No `u=a1` segment, relative, or javascript: all skip.
        assert_eq!(unwrap_bing_url("https://www.bing.com/ck/a?p=x"), None);
        assert_eq!(unwrap_bing_url("/relative/path"), None);
        assert_eq!(unwrap_bing_url("javascript:void(0)"), None);
        assert_eq!(unwrap_bing_url(""), None);
    }

    /// Regression: before status-only 429 detection existed, a naive body
    /// check on this synthetic challenge-marker body (documented shape per
    /// public Bing-scraping research, see the module doc) would parse it
    /// as `Ok(vec![])` (zero `li.b_algo` rows -- there are none), silently
    /// treating a block as "no results" instead of a typed `Challenge`.
    /// Shows the red run: `map_bing_response` without the marker check
    /// would fall through to `Upstream`/`Ok` depending on status, never
    /// `Challenge`, on this exact body.
    #[test]
    fn bing_challenge_verify_marker_maps_challenge_on_status_200() {
        let body = r#"<html><body><script>window.appInsights={"verifyEndpoint": "https://www.bing.com/challenge/verify?partner=7&token=","captchaSuccessPostMessage":"verificationComplete"};</script></body></html>"#;
        assert!(is_bing_challenge(body));
        let err = map_bing_response(200, body, "q", None).unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
    }

    #[test]
    fn bing_429_maps_challenge() {
        let err = map_bing_response(429, "no marker here", "q", None).unwrap_err();
        assert_eq!(err.http_status(), 429);
    }

    #[test]
    fn bing_ordinary_result_page_does_not_trigger_challenge() {
        let html = include_str!(
            "../../../docs/research/search-engines/fixtures/bing/Q1-success-mktUS.html"
        );
        assert!(!is_bing_challenge(html));
    }

    #[test]
    fn bing_500_maps_upstream_503() {
        let err = map_bing_response(500, "boom", "q", None).unwrap_err();
        assert_eq!(err.http_status(), 503);
        assert_eq!(err.code(), "upstream");
    }

    #[test]
    fn setlang_derives_language_subtag() {
        assert_eq!(setlang_from_market("en-US"), "en");
        assert_eq!(setlang_from_market("pt-BR"), "pt");
        assert_eq!(setlang_from_market("fr"), "fr");
    }

    #[tokio::test]
    async fn bing_always_sends_mkt_and_setlang() {
        let stub = StubServer::serve(|path: &str, _: &str| {
            assert!(path.contains("mkt="), "mkt missing from request: {path}");
            assert!(
                path.contains("setlang="),
                "setlang missing from request: {path}"
            );
            StubReply::text(200, "<ol id=\"b_results\"></ol>")
        })
        .await;
        let client = reqwest::Client::new();
        std::env::remove_var("SEARCH_BING_MARKET");
        let rows = bing_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .expect("stub replies 200");
        assert_eq!(rows.len(), 0);
        assert_eq!(stub.hits("/search"), 1);
    }

    #[tokio::test]
    async fn bing_market_env_override_changes_setlang() {
        // The path itself is the evidence: echo it into the body so the
        // assertion can inspect exactly what `bing_get` sent, with no need
        // for a capturing stub.
        let stub = StubServer::serve(|path: &str, _: &str| {
            StubReply::text(200, &format!("<!--{path}--><ol id=\"b_results\"></ol>"))
        })
        .await;
        std::env::set_var("SEARCH_BING_MARKET", "pt-BR");
        let client = reqwest::Client::new();
        let mkt = bing_market();
        let setlang = setlang_from_market(&mkt);
        assert_eq!(mkt, "pt-BR");
        assert_eq!(setlang, "pt");
        let (status, body, _) = bing_get(
            &client,
            &format!("{}/search", stub.base()),
            "q",
            &mkt,
            &setlang,
            &crate::web::profile::pick_profile(),
        )
        .await
        .expect("stub replies 200");
        assert_eq!(status, 200);
        assert!(
            body.contains("mkt=pt-BR"),
            "path echo missing mkt=pt-BR: {body}"
        );
        assert!(
            body.contains("setlang=pt"),
            "path echo missing setlang=pt: {body}"
        );
        std::env::remove_var("SEARCH_BING_MARKET");
    }

    #[tokio::test]
    async fn bing_challenge_costs_one_get() {
        let stub = StubServer::serve(|_: &str, _: &str| {
            StubReply::text(
                200,
                r#"<script>"verifyEndpoint":"https://www.bing.com/challenge/verify"</script>"#,
            )
        })
        .await;
        let client = reqwest::Client::new();
        let err = bing_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(stub.hits("/search"), 1);
    }

    #[tokio::test]
    async fn bing_slow_stub_times_out() {
        let stub = StubServer::serve_slow(std::time::Duration::from_secs(5)).await;
        let client = reqwest::Client::new();
        let err = bing_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .expect_err("slow stub must time out at leg level");
        assert_eq!(err.http_status(), 504);
    }

    #[test]
    fn bing_clamp_single_page() {
        let mut html = String::from("<ol id=\"b_results\">");
        for i in 0..25 {
            html.push_str(&format!(
                r#"<li class="b_algo"><h2><a href="https://example.com/s{i}">Title {i}</a></h2><div class="b_caption"><p>snippet {i}</p></div></li>"#
            ));
        }
        html.push_str("</ol>");
        let rows = parse_bing_html(&html, "q");
        assert_eq!(rows.len(), 20);
    }
}
