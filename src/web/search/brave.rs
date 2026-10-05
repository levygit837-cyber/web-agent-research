//! Brave HTML leg of the fetch-only web search engine.
//!
//! Plain GET with Brave's own default cookies, DOM-parsed (`scraper`) into
//! raw hits for the agent-loop Queries. Unwrapped direct URLs, no redirect
//! wrapper. Selector drift and rate-limit changes land here.
//!
//! Request shape, cookies, and the status-only 429 detection are all
//! measured and fixture-backed in `docs/research/search-engines.md`
//! ("Brave HTML -- KEEP") and its `fixtures/brave/`; no code is ported
//! from SearXNG's own `brave.py` (AGPL-3.0, idea only -- see
//! `THIRD-PARTY-NOTICES.md`).

use scraper::{Html, Selector};

use super::fanout::{map_transport_error, parse_retry_after};
use crate::web::profile::{pick_profile, BrowserProfile};
use crate::web::search::decode::collapse_whitespace;
use crate::web::search::markup::rows_or_drift;
use crate::web::search::types::{
    Recency, SearchProvider, SearchProviderError, SearchResult, MAX_NUM_RESULTS,
};
use crate::web::search::{apply_navigation_headers, leg_timeout, ChainPosition};

/// Brave HTML search endpoint (#66). Tests override via
/// `brave_search_with_base`.
pub const BRAVE_SEARCH_URL: &str = "https://search.brave.com/search";

/// Brave's own default cookies (measured 2026-09-28, `docs/research/
/// search-engines.md`: "matching SearXNG's own `brave.py::request`" --
/// idea only, no code copied). Sent verbatim on every request; a cold
/// first-touch request with no prior session needs no other cookie to get
/// real results.
pub const BRAVE_COOKIE: &str =
    "safesearch=off; useLocation=0; summarizer=0; country=us; ui_lang=en-us";

/// Map a finished Brave HTTP exchange to rows or a typed leg error: HTTP
/// `429` -> `Challenge` unconditionally (measured: the 429 body is
/// byte-identical to Brave's normal SvelteKit app shell -- CloudFront edge
/// rate-limiting with no unique block marker, so detection is
/// status-code-only, never body-based); other non-2xx -> `Upstream` (503,
/// carrying the real `status` and any parsed `retry_after_secs` for #62's
/// suspension mapping). Empty with no marker is `Ok(vec![])` (zero results
/// is not an error).
pub(crate) fn map_brave_response(
    status: u16,
    body: &str,
    query: &str,
    retry_after_secs: Option<u64>,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    if status == 429 {
        return Err(SearchProviderError::Challenge {
            provider: SearchProvider::Brave,
            detail: "Brave rate-limited the request (HTTP 429, CloudFront edge). Retry via an accredited provider or later.".to_string(),
        });
    }
    if !(200..300).contains(&status) {
        return Err(SearchProviderError::Upstream {
            provider: SearchProvider::Brave,
            detail: format!("Brave HTML error ({status})"),
            status: Some(status),
            retry_after_secs,
        });
    }
    rows_or_drift(
        SearchProvider::Brave,
        status,
        body,
        parse_brave_html(body, query),
        is_brave_serp,
    )
}

/// `true` when a Brave page is recognizably a results page (#104): the
/// `id="results"` container (present in the live success fixture, absent
/// from the 429 SvelteKit shell and from the blocked probe pages).
pub fn is_brave_serp(body: &str) -> bool {
    body.contains("id=\"results\"")
}

/// One Brave GET: `q`/`source=web` query params, the default cookie header,
/// desktop navigation headers, per-request timeout ceiling. Returns status +
/// body + any parsed `Retry-After` header (#62); transport errors map to
/// `Timeout` (504, `is_timeout`) or `Upstream` (503).
async fn brave_get(
    client: &reqwest::Client,
    url: &str,
    query: &str,
    profile: &BrowserProfile,
) -> Result<(u16, String, Option<u64>), SearchProviderError> {
    let response = apply_navigation_headers(client.get(url), profile, ChainPosition::First, None)
        .query(&[("q", query), ("source", "web")])
        .header("Cookie", BRAVE_COOKIE)
        .timeout(leg_timeout())
        .send()
        .await
        .map_err(|err| {
            map_transport_error(
                SearchProvider::Brave,
                query,
                err.is_timeout(),
                format!("Brave request failed: {err}"),
            )
        })?;
    let status = response.status().as_u16();
    let retry_after = parse_retry_after(&response);
    let text = response.text().await.map_err(|err| {
        map_transport_error(
            SearchProvider::Brave,
            query,
            err.is_timeout(),
            format!("Brave body read failed: {err}"),
        )
    })?;
    Ok((status, text, retry_after))
}

/// Brave leg: single GET, page 1 only (#66 human decision; pagination via
/// `offset=N` is #73's job).
pub(crate) async fn brave_search(
    client: &reqwest::Client,
    query: &str,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    // One profile per leg (#59), matching every other engine's pattern
    // even though Brave's single GET has no follow-up request to reuse it.
    let profile = pick_profile();
    let (status, body, retry_after) = brave_get(client, base, query, &profile).await?;
    map_brave_response(status, &body, query, retry_after)
}

/// Parse Brave HTML (DOM via `scraper` -- measured shape: `div.result-wrapper`
/// rows, direct unwrapped `<a href>`, title in `.title.search-snippet-title`,
/// snippet in `.generic-snippet .content`). Anchors on the stable semantic
/// classes, not the Svelte content-hash suffix (`svelte-14r20fy` etc.),
/// which drifts on Brave redeploys per the #65 evaluation.
pub fn parse_brave_html(html: &str, query: &str) -> Vec<SearchResult> {
    let document = Html::parse_document(html);
    let row_sel = Selector::parse("div.result-wrapper").expect("static row selector");
    let link_sel = Selector::parse("div.result-content > a").expect("static link selector");
    let title_sel = Selector::parse(".title.search-snippet-title").expect("static title selector");
    let snippet_sel =
        Selector::parse(".generic-snippet .content").expect("static snippet selector");
    let mut rows = Vec::new();
    for row in document.select(&row_sel) {
        let Some(link) = row.select(&link_sel).next() else {
            continue;
        };
        let href = link.value().attr("href").unwrap_or("").trim();
        if !(href.starts_with("http://") || href.starts_with("https://")) {
            continue;
        }
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
            url: href.to_string(),
            snippet,
            provider: SearchProvider::Brave,
            query: query.to_string(),
            rank: rows.len(),
            published_date: None,
        });
    }
    rows
}

#[doc(hidden)]
pub async fn brave_search_with_base(
    client: &reqwest::Client,
    query: &str,
    _recency: Option<Recency>,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    // Brave's `tf` window is unverified (#81: the one live request got an
    // HTTP 429, see `SearchProvider::applies_recency`), so the fan-out never
    // dispatches a Brave leg when `recency` is set. The parameter is kept
    // for signature symmetry with the other engine legs and is ignored.
    brave_search(client, query, base).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::test_support::{StubReply, StubServer};

    /// Real fixture-backed parse (docs/research/search-engines/fixtures/
    /// brave/Q1-success.html): 4 organic rows, direct unwrapped URLs,
    /// title/snippet extracted via the stable semantic classes.
    #[test]
    fn brave_parse_real_fixture_direct_urls() {
        let html =
            include_str!("../../../docs/research/search-engines/fixtures/brave/Q1-success.html");
        let rows = parse_brave_html(html, "tokio rust crate documentation");
        assert_eq!(
            rows.len(),
            4,
            "fixture holds 4 organic result rows: {rows:?}"
        );
        assert_eq!(rows[0].url, "https://crates.io/crates/tokio");
        assert_eq!(rows[0].title, "tokio - crates.io: Rust Package Registry");
        assert!(rows[0].snippet.contains("Tokio leverages Rust"));
        assert_eq!(rows[0].provider, SearchProvider::Brave);
        assert_eq!(rows[0].rank, 0);
        assert_eq!(rows[1].url, "https://docs.rs/tokio");
        assert_eq!(rows[1].title, "tokio - Rust");
        assert_eq!(
            rows[2].url,
            "https://tokio-rs.github.io/tokio-proto/tokio_proto/index.html"
        );
        assert_eq!(
            rows[3].url,
            "https://generalistprogrammer.com/tutorials/tokio-rust-crate-guide"
        );
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.rank, i);
            assert_eq!(row.query, "tokio rust crate documentation");
        }
    }

    /// Regression: the real 429 fixture (docs/research/search-engines/
    /// fixtures/brave/Q8-429-blocked.html) is byte-identical to Brave's
    /// normal SvelteKit shell -- no body marker exists, so detection must
    /// be status-code-only. Shows the red run: before this leg's status
    /// check existed, a naive body-marker check (matching DDG/Startpage's
    /// pattern) would parse this fixture as `Ok(vec![])`, not `Challenge`.
    #[test]
    fn brave_429_maps_challenge_with_no_body_marker() {
        let html = include_str!(
            "../../../docs/research/search-engines/fixtures/brave/Q8-429-blocked.html"
        );
        // Prove there is genuinely no marker a body-only check could catch
        // in the SERVED markup -- strip every `<!-- ... -->` span first
        // (the fixture's own explanatory comments legitimately mention
        // "blocked"/"429"/"rate limit" for humans; those bytes were never
        // part of what Brave actually sent on the wire).
        let mut served_markup = html.to_string();
        while let Some(start) = served_markup.find("<!--") {
            let Some(end) = served_markup[start..].find("-->") else {
                break;
            };
            served_markup.replace_range(start..start + end + "-->".len(), "");
        }
        let lower = served_markup.to_ascii_lowercase();
        assert!(
            !lower.contains("blocked") && !lower.contains("rate limit") && !lower.contains("429"),
            "served markup must carry no textual block marker, confirming status-only detection is required: {served_markup}"
        );
        let err = map_brave_response(429, html, "cargo", None).unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
    }

    #[test]
    fn brave_200_with_challenge_body_shape_would_wrongly_parse_without_status_check() {
        // Belt-and-suspenders: even the un-429'd shell body has zero
        // organic result-wrapper rows, so a caller that only inspected the
        // body (no status) would still (correctly, if accidentally) see
        // zero rows rather than fabricated hits -- but the typed Challenge
        // requires the status check, proven above.
        let html = include_str!(
            "../../../docs/research/search-engines/fixtures/brave/Q8-429-blocked.html"
        );
        let rows = parse_brave_html(html, "cargo");
        assert!(rows.is_empty());
    }

    #[test]
    fn brave_500_maps_upstream_503() {
        let err = map_brave_response(500, "boom", "q", None).unwrap_err();
        assert_eq!(err.http_status(), 503);
        assert_eq!(err.code(), "upstream");
    }

    #[tokio::test]
    async fn brave_sends_q_source_and_cookie() {
        let (stub, captured) = StubServer::serve_capturing_headers().await;
        let client = reqwest::Client::new();
        // The header-capture stub replies with a DDG-shaped page (no Brave
        // `id="results"` container), so the leg maps it to the typed
        // unrecognized-markup error (#104) -- the request itself, which
        // is what this test checks, was still sent exactly once.
        let err = brave_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .expect_err("a page without Brave's container is drift");
        assert!(
            matches!(&err, SearchProviderError::Upstream { detail, status: Some(200), .. } if detail == "unrecognized markup"),
            "{err:?}"
        );
        let requests = captured.lock().expect("captured");
        assert_eq!(requests.len(), 1, "Brave is a single GET, no follow-up");
        let names = super::super::test_support::header_names(&requests[0]);
        assert!(names.contains(&"cookie".to_string()));
        assert_eq!(stub.hits("/search"), 1);
    }

    #[tokio::test]
    async fn brave_challenge_costs_one_get() {
        let stub = StubServer::serve(|_: &str, _: &str| StubReply::text(429, "shell")).await;
        let client = reqwest::Client::new();
        let err = brave_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(stub.hits("/search"), 1);
    }

    #[tokio::test]
    async fn brave_slow_stub_times_out() {
        let stub = StubServer::serve_slow(std::time::Duration::from_secs(5)).await;
        let client = reqwest::Client::new();
        let err = brave_search_with_base(&client, "q", None, &format!("{}/search", stub.base()))
            .await
            .expect_err("slow stub must time out at leg level");
        assert_eq!(err.http_status(), 504);
    }

    #[test]
    fn brave_clamp_single_page() {
        let mut html = String::new();
        for i in 0..25 {
            html.push_str(&format!(
                r#"<div class="result-wrapper"><div class="result-content"><a href="https://example.com/s{i}" class="l1"><div class="title search-snippet-title">T{i}</div></a><div class="generic-snippet"><div class="content">d{i}</div></div></div></div>"#
            ));
        }
        let rows = parse_brave_html(&html, "q");
        assert_eq!(rows.len(), 20);
    }

    #[test]
    fn brave_skips_row_without_link_or_empty_title() {
        let html = r#"
<div class="result-wrapper"><div class="result-content">
  <a href="https://example.com/ok" class="l1"><div class="title search-snippet-title">Real Title</div></a>
  <div class="generic-snippet"><div class="content">real snippet</div></div>
</div></div>
<div class="result-wrapper"><div class="result-content">
  <a href="https://example.com/empty-title" class="l1"><div class="title search-snippet-title"></div></a>
</div></div>
<div class="result-wrapper"><div class="result-content">
  <a href="javascript:void(0)" class="l1"><div class="title search-snippet-title">Not a link</div></a>
</div></div>
"#;
        let rows = parse_brave_html(html, "q");
        assert_eq!(
            rows.len(),
            1,
            "empty-title and non-http rows skipped: {rows:?}"
        );
        assert_eq!(rows[0].url, "https://example.com/ok");
        assert_eq!(rows[0].snippet, "real snippet");
    }
}
