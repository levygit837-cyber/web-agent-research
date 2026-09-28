//! Startpage leg of the fetch-only web search engine.
//!
//! Homepage token fetch, verbatim form POST with direct-GET fallback,
//! DOM parse into raw hits for the agent-loop Queries. Selector drift
//! and bot-wall changes land here.
//!
//! Ported from oh-my-pi (MIT, can1357/oh-my-pi@83c9df0); see
//! `THIRD-PARTY-NOTICES.md`.

use std::collections::HashSet;

use scraper::{Html, Selector};

use super::fanout::map_transport_error;
use crate::web::profile::{pick_profile, BrowserProfile};
use crate::web::search::decode::{collapse_whitespace, percent_encode};
use crate::web::search::types::{
    Recency, SearchProvider, SearchProviderError, SearchResult, MAX_NUM_RESULTS,
};
use crate::web::search::{apply_navigation_headers, leg_timeout, request_origin, ChainPosition};

/// Startpage homepage for anti-bot `sc` token extraction (Omp `fetchFormInputs`).
pub const STARTPAGE_HOME_URL: &str = "https://www.startpage.com/";

/// Startpage search POST target (Omp `STARTPAGE_SEARCH_URL`).
pub const STARTPAGE_SEARCH_URL: &str = "https://www.startpage.com/sp/search";

/// Startpage bot-wall detector (Omp `isChallengeResponse`, extended #53):
/// a redirect to `/en/errors/` or `/sp/captcha` (the live `/sp/captcha-block`
/// redirect form observed 2026-09-08 is subsumed by the `/sp/captcha`
/// prefix), a final URL under `/sp/cdn/error-pages/blocked` (the curl-UA
/// redirect target observed 2026-09-08), the `component---src-pages-captcha`
/// CAPTCHA-page marker, or the Anubis proof-of-work challenge page (live
/// body captured 2026-09-27): the `id="anubis_challenge"` script element or
/// the `/.within.website/x/cmd/anubis/` script-src prefix. Both Anubis
/// markers are stable HTML structure, never the bare word `anubis` (a result
/// snippet may legitimately mention the project). A bare `"captcha"`
/// substring MUST NOT trigger either (captcha-topic query snippets would
/// false-positive).
pub fn is_startpage_challenge(body: &str, final_url: &str) -> bool {
    final_url.contains("/en/errors/")
        || final_url.contains("/sp/captcha")
        || final_url.contains("/sp/cdn/error-pages/blocked")
        || body.contains("component---src-pages-captcha")
        || body.contains(r#"id="anubis_challenge""#)
        || body.contains("/.within.website/x/cmd/anubis/")
}

/// Map a finished Startpage HTTP exchange to rows or a typed leg error:
/// challenge signals -> `Challenge` (429); non-2xx without markers ->
/// `Upstream` (503); otherwise parse (possibly empty).
pub(crate) fn map_startpage_response(
    status: u16,
    body: &str,
    final_url: &str,
    query: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    if is_startpage_challenge(body, final_url) {
        return Err(SearchProviderError::Challenge {
            provider: SearchProvider::Startpage,
            detail: "Startpage blocked the request with a bot-detection challenge (Anubis proof-of-work or CAPTCHA). Retry via an accredited provider or later.".to_string(),
        });
    }
    if !(200..300).contains(&status) {
        return Err(SearchProviderError::Upstream {
            provider: SearchProvider::Startpage,
            detail: format!("Startpage HTML error ({status})"),
        });
    }
    Ok(parse_startpage_html(body, query))
}

/// Homepage fetch outcome: token inputs, a reached-but-no-token miss (the
/// origin answered -- non-OK status, homepage challenge wall, markup
/// drift -- so the fallback below is a same-origin follow-up with a
/// Referer), or a transport failure (the origin was never reached, so the
/// fallback below is really the chain's first navigation: `none`, no
/// Referer). The fallback body re-runs challenge detection, so a live wall
/// still surfaces as `Challenge` (429).
enum FormFetch {
    Inputs(Vec<(String, String)>),
    Miss,
    Unreached,
}

/// Startpage form fetch (Omp `fetchFormInputs`, best effort): GET the homepage
/// and extract hidden inputs incl. the anti-bot `sc` token. Failures, non-OK
/// status, a homepage challenge, and markup drift (no `sc`) are all
/// `FormFetch::Miss` and degrade to the direct-GET fallback.
async fn fetch_startpage_form_inputs(
    client: &reqwest::Client,
    home_url: &str,
    query: &str,
    profile: &BrowserProfile,
) -> FormFetch {
    let _ = query;
    let Ok(response) =
        apply_navigation_headers(client.get(home_url), profile, ChainPosition::First, None)
            .timeout(leg_timeout())
            .send()
            .await
    else {
        return FormFetch::Unreached;
    };
    if !response.status().is_success() {
        return FormFetch::Miss;
    }
    let final_url = response.url().to_string();
    let Ok(body) = response.text().await else {
        return FormFetch::Miss;
    };
    if is_startpage_challenge(&body, &final_url) {
        return FormFetch::Miss;
    }
    match parse_search_form_inputs(&body) {
        Some(inputs) => FormFetch::Inputs(inputs),
        None => FormFetch::Miss,
    }
}

/// Startpage search POST: hidden inputs echoed verbatim + `query`
/// (`+with_date` when recency is set). Always a follow-up: it only runs
/// after a homepage GET that reached the origin and returned a token
/// (#59/#58 chain).
async fn startpage_post(
    client: &reqwest::Client,
    url: &str,
    inputs: &[(String, String)],
    query: &str,
    recency: Option<Recency>,
    profile: &BrowserProfile,
) -> Result<(u16, String, String), SearchProviderError> {
    let mut form: Vec<(String, String)> = inputs.to_vec();
    form.push(("query".to_string(), query.to_string()));
    if let Some(recency) = recency {
        form.push(("with_date".to_string(), recency.param().to_string()));
    }
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut builder = apply_navigation_headers(
        client.post(url),
        profile,
        ChainPosition::FollowUp,
        Some(STARTPAGE_HOME_URL),
    );
    if let Some(origin) = request_origin(url) {
        builder = builder.header("Origin", origin);
    }
    let response = builder
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(leg_timeout())
        .send()
        .await
        .map_err(|err| {
            map_transport_error(
                SearchProvider::Startpage,
                query,
                err.is_timeout(),
                format!("Startpage request failed: {err}"),
            )
        })?;
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let text = response.text().await.map_err(|err| {
        map_transport_error(
            SearchProvider::Startpage,
            query,
            err.is_timeout(),
            format!("Startpage body read failed: {err}"),
        )
    })?;
    Ok((status, text, final_url))
}

/// Startpage direct-GET fallback (Omp "best effort ... falls back to a direct
/// GET"): `query`/`with_date` on the search URL. One fallback only.
/// `chain_position`/`referer` come from whether the earlier homepage GET
/// reached the origin (`FollowUp` + `Referer: STARTPAGE_HOME_URL`) or never
/// did (`First`, no Referer -- this GET is then the chain's real first
/// navigation).
async fn startpage_get(
    client: &reqwest::Client,
    url: &str,
    query: &str,
    recency: Option<Recency>,
    profile: &BrowserProfile,
    chain_position: ChainPosition,
    referer: Option<&str>,
) -> Result<(u16, String, String), SearchProviderError> {
    let mut request = apply_navigation_headers(client.get(url), profile, chain_position, referer)
        .query(&[("query", query)])
        .timeout(leg_timeout());
    if let Some(recency) = recency {
        request = request.query(&[("with_date", recency.param())]);
    }
    let response = request.send().await.map_err(|err| {
        map_transport_error(
            SearchProvider::Startpage,
            query,
            err.is_timeout(),
            format!("Startpage request failed: {err}"),
        )
    })?;
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let text = response.text().await.map_err(|err| {
        map_transport_error(
            SearchProvider::Startpage,
            query,
            err.is_timeout(),
            format!("Startpage body read failed: {err}"),
        )
    })?;
    Ok((status, text, final_url))
}

/// Startpage leg: token path (homepage GET -> verbatim POST), else direct-GET
/// fallback. No pagination: single page sliced to `MAX_NUM_RESULTS` by the
/// parser.
pub(crate) async fn startpage_search(
    client: &reqwest::Client,
    query: &str,
    recency: Option<Recency>,
    home_url: &str,
    search_url: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    // One profile per chain (#59): homepage GET, search POST/GET fallback
    // all share it, matching a real browser tab.
    let profile = pick_profile();
    let (fallback_position, fallback_referer) =
        match fetch_startpage_form_inputs(client, home_url, query, &profile).await {
            FormFetch::Inputs(inputs) => {
                let (status, body, final_url) =
                    startpage_post(client, search_url, &inputs, query, recency, &profile).await?;
                return map_startpage_response(status, &body, &final_url, query);
            }
            // The homepage GET reached the origin (even if it 404'd,
            // challenged, or had no `sc`): the fallback is same-origin.
            FormFetch::Miss => (ChainPosition::FollowUp, Some(STARTPAGE_HOME_URL)),
            // The homepage GET never reached the origin: the fallback is
            // really the chain's first navigation.
            FormFetch::Unreached => (ChainPosition::First, None),
        };
    let (status, body, final_url) = startpage_get(
        client,
        search_url,
        query,
        recency,
        &profile,
        fallback_position,
        fallback_referer,
    )
    .await?;
    map_startpage_response(status, &body, &final_url, query)
}

/// Keep only absolute http(s) URLs outside `startpage.com` (Omp
/// `sanitizeResultUrl`): exit URLs are direct, no wrapper. Anything else
/// returns `None` (row skipped).
pub fn sanitize_startpage_url(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return None;
    }
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return None;
    };
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    if host == "startpage.com" || host.ends_with(".startpage.com") {
        return None;
    }
    Some(trimmed.to_string())
}

/// Parse Startpage HTML (pure; DOM via the `scraper` crate -- Omp
/// `parseHtmlResults` via `parseHTML`): iterate `div.result` rows, skipping
/// the `a-bg-result` anti-adblock honeypot div; title from `a.result-link`
/// (`h2.wgl-title`, falling back to `h2, h3`); snippet from `p.description`
/// (else `""`).
pub fn parse_startpage_html(html: &str, query: &str) -> Vec<SearchResult> {
    let document = Html::parse_document(html);
    let row_sel = Selector::parse("div.result").expect("static row selector");
    let link_sel = Selector::parse("a.result-link").expect("static link selector");
    let title_primary = Selector::parse("h2.wgl-title").expect("static primary title selector");
    let title_fallback = Selector::parse("h2, h3").expect("static title selector");
    let snippet_sel = Selector::parse("p.description").expect("static snippet selector");
    let mut rows = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for row in document.select(&row_sel) {
        if has_honeypot_ancestor(&row) {
            continue;
        }
        let Some(link) = row.select(&link_sel).next() else {
            continue;
        };
        let href = link.value().attr("href").unwrap_or("");
        let Some(url) = sanitize_startpage_url(href) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        let title = link
            .select(&title_primary)
            .next()
            .or_else(|| link.select(&title_fallback).next())
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
            provider: SearchProvider::Startpage,
            query: query.to_string(),
            rank: rows.len(),
            published_date: None,
        });
    }
    rows
}

fn has_honeypot_ancestor(row: &scraper::ElementRef<'_>) -> bool {
    for ancestor in row.ancestors() {
        if let Some(element) = ancestor.value().as_element() {
            if element.has_class(
                "a-bg-result",
                scraper::CaseSensitivity::AsciiCaseInsensitive,
            ) {
                return true;
            }
        }
    }
    false
}

/// Extract Startpage search-form hidden inputs (Omp `parseSearchFormInputs`):
/// the hidden `<input>`s of `form[action="/sp/search"]`, verbatim. Requires
/// the anti-bot `sc` token: `None` without non-empty `sc` (exactly like Omp
/// returning `undefined`), which routes the leg to the direct-GET fallback.
pub fn parse_search_form_inputs(html: &str) -> Option<Vec<(String, String)>> {
    let document = Html::parse_document(html);
    let form_sel = Selector::parse(r#"form[action="/sp/search"]"#).expect("static form selector");
    let input_sel = Selector::parse("input").expect("static input selector");
    for form in document.select(&form_sel) {
        let mut inputs = Vec::new();
        for input in form.select(&input_sel) {
            let element = input.value();
            let is_hidden = element.attr("type").unwrap_or("") == "hidden";
            let (Some(name), Some(value)) = (element.attr("name"), element.attr("value")) else {
                continue;
            };
            if is_hidden || name == "sc" {
                inputs.push((name.to_string(), value.to_string()));
            }
        }
        if inputs.iter().any(|(k, v)| k == "sc" && !v.is_empty()) {
            return Some(inputs);
        }
    }
    None
}

#[doc(hidden)]
pub async fn startpage_search_with_base(
    client: &reqwest::Client,
    query: &str,
    recency: Option<Recency>,
    home_url: &str,
    search_url: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    startpage_search(client, query, recency, home_url, search_url).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::test_support::{StubReply, StubServer};

    #[test]
    fn sp_parse_primary_selectors() {
        let html = r#"
<div class="result">
  <a class="result-link" href="https://example.com/sp-first"><h2 class="wgl-title">SP <b>First</b></h2></a>
  <p class="description">sp first snippet here</p>
</div>
<div class="result">
  <a class="result-link" href="https://example.com/sp-second"><h3>Second Header</h3></a>
  <p class="description">second text</p>
</div>
<div class="a-bg-result">
  <a class="result-link" href="https://example.com/honeypot"><h2 class="wgl-title">Honeypot</h2></a>
  <p class="description">must be ignored</p>
</div>
<div class="a-bg-result"><div class="result">
  <a class="result-link" href="https://example.com/nested-honeypot"><h2 class="wgl-title">Nested</h2></a>
  <p class="description">nested walk must skip this row</p>
</div></div>
<div class="result">
  <a class="result-link" href="https://example.com/no-snippet"><h2 class="wgl-title">No Snippet</h2></a>
</div>
"#;
        let rows = parse_startpage_html(html, "sp query");
        assert_eq!(rows.len(), 3, "honeypot skipped, got {rows:?}");
        assert_eq!(rows[0].title, "SP First");
        assert_eq!(rows[0].url, "https://example.com/sp-first");
        assert_eq!(rows[0].snippet, "sp first snippet here");
        assert_eq!(rows[0].provider, SearchProvider::Startpage);
        assert_eq!(rows[0].query, "sp query");
        assert_eq!(rows[1].title, "Second Header");
        assert_eq!(rows[2].snippet, "");
    }

    #[test]
    fn sp_clamp_single_page() {
        // 25-row fixture -> <=20, no pagination, ranks stay document order.
        let mut html = String::new();
        for i in 0..25 {
            html.push_str(&format!(
                r#"<div class="result"><a class="result-link" href="https://example.com/s{i}"><h2 class="wgl-title">T{i}</h2></a><p class="description">d{i}</p></div>"#
            ));
        }
        let rows = parse_startpage_html(&html, "q");
        assert_eq!(rows.len(), 20);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.rank, i);
        }
    }

    #[test]
    fn sp_sanitize_rejects_internal() {
        assert_eq!(
            sanitize_startpage_url("https://example.com/ok"),
            Some("https://example.com/ok".to_string())
        );
        assert_eq!(
            sanitize_startpage_url("https://www.startpage.com/sp/search?q=x"),
            None
        );
        assert_eq!(sanitize_startpage_url("https://sub.startpage.com/y"), None);
        assert_eq!(sanitize_startpage_url("/relative/path"), None);
        assert_eq!(sanitize_startpage_url("javascript:void(0)"), None);
        assert_eq!(sanitize_startpage_url("ftp://example.com/x"), None);
    }

    #[test]
    fn sp_captcha_marker_maps_429() {
        let err = map_startpage_response(
            200,
            "xx component---src-pages-captcha yy",
            "https://www.startpage.com/sp/search",
            "q",
        )
        .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
    }

    #[test]
    fn bare_captcha_word_does_not_trigger() {
        // Captcha-topic snippet text is NOT a challenge: parses (possibly
        // empty) instead of mapping to 429.
        let rows = map_startpage_response(
            200,
            "how do captchas work? explained",
            "https://www.startpage.com/sp/search",
            "q",
        )
        .expect("bare captcha word parses, not a challenge");
        assert!(rows.is_empty());
    }

    /// Trimmed excerpt of the live Startpage Anubis PoW challenge body
    /// captured 2026-09-27 (`/tmp/war-recon/startpage-anubis.html`, residential
    /// IP 187.19.223.83, AS28126 Brisanet BR): the two stable markers kept
    /// verbatim, surrounding cosmetic HTML/CSS/JS dropped (#53).
    const ANUBIS_CHALLENGE_EXCERPT: &str = concat!(
        r#"<script id="anubis_version" type="application/json">"v1.26.4"</script>"#,
        r#"<script id="anubis_challenge" type="application/json">{"rules":{"algorithm":"fast","difficulty":6},"challenge":{"issuedAt":"2026-09-27T20:41:38.253817028Z","id":"01a0e49a-14cd-7c6b-8599-7159692835bb","method":"fast","difficulty":6,"spent":false}}</script>"#,
        r#"<div class="sp-message">Verifying your request...</div>"#,
        r#"<script id="anubis-main" defer type="module" src="/.within.website/x/cmd/anubis/static/js/main.mjs?cacheBuster=v1.26.4"></script>"#,
    );

    #[test]
    fn sp_anubis_pow_marker_maps_429_with_pow_detail() {
        // Regression (#53): the live Anubis proof-of-work page has none of
        // the older markers (no `/sp/captcha` redirect, no
        // `component---src-pages-captcha`), so it used to parse as 0 rows
        // instead of mapping to Challenge.
        let err = map_startpage_response(
            200,
            ANUBIS_CHALLENGE_EXCERPT,
            "https://www.startpage.com/sp/search",
            "q",
        )
        .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
        let detail = match &err {
            SearchProviderError::Challenge { detail, .. } => detail.clone(),
            other => panic!("expected Challenge, got {other:?}"),
        };
        assert!(
            detail.contains("Anubis") && detail.contains("proof-of-work"),
            "challenge detail must name Anubis PoW, not only CAPTCHA: {detail}"
        );
    }

    #[test]
    fn sp_blocked_cdn_final_url_maps_429() {
        // Regression (#53): curl-UA/empty-UA requests redirect to the CDN
        // blocked page instead of serving Anubis or a captcha-block form.
        let err = map_startpage_response(
            200,
            "<html>blocked</html>",
            "https://cdn.startpage.com/sp/cdn/error-pages/blocked.html",
            "q",
        )
        .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
    }

    #[test]
    fn bare_anubis_word_in_result_snippet_does_not_trigger() {
        // Regression (#53): a normal Startpage result page whose snippet
        // mentions the Anubis project by name must NOT be mistaken for the
        // Anubis challenge page itself — only the stable structural markers
        // (`id="anubis_challenge"`, the `/.within.website/x/cmd/anubis/`
        // script src) count.
        let html = r#"<div class="result">
  <a class="result-link" href="https://example.com/anubis-guide"><h2 class="wgl-title">Anubis bot-wall guide</h2></a>
  <p class="description">Anubis is a proof-of-work challenge used by some sites to block bots.</p>
</div>"#;
        assert!(!is_startpage_challenge(html, "https://x/"));
        let rows = map_startpage_response(200, html, "https://www.startpage.com/sp/search", "q")
            .expect("bare Anubis mention parses, not a challenge");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Anubis bot-wall guide");
    }

    #[test]
    fn sp_token_extraction() {
        let html = r#"<form action="/sp/search" method="post">
<input type="hidden" name="sc" value="token123"/>
<input type="hidden" name="lang" value="english"/>
</form>"#;
        let inputs = parse_search_form_inputs(html).expect("sc present");
        assert!(inputs.iter().any(|(k, v)| k == "sc" && v == "token123"));
        let missing = r#"<form action="/sp/search" method="post">
<input type="hidden" name="lang" value="english"/>
</form>"#;
        assert_eq!(parse_search_form_inputs(missing), None);
        assert_eq!(parse_search_form_inputs("<div>no form</div>"), None);
    }

    #[tokio::test]
    async fn startpage_challenge_costs_one_post() {
        let stub = StubServer::serve(|path: &str, _body: &str| {
            if path == "/" || path.starts_with("/?") {
                StubReply::text(
                    200,
                    "<form action=\"/sp/search\" method=\"post\"><input type=\"hidden\" name=\"sc\" value=\"tok\"/></form>",
                )
            } else {
                StubReply::text(200, "xx component---src-pages-captcha yy")
            }
        })
        .await;
        let client = reqwest::Client::new();
        let err = startpage_search_with_base(
            &client,
            "q",
            None,
            &format!("{}/", stub.base()),
            &format!("{}/sp/search", stub.base()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(
            stub.hits("/sp/search"),
            1,
            "challenge costs exactly one POST"
        );
    }

    #[tokio::test]
    async fn startpage_homepage_challenge_maps_429() {
        // Homepage answers the challenge wall: the leg degrades to the GET
        // fallback, and the fallback wall body re-triggers challenge
        // detection, so the leg still reports Challenge (429).
        let stub = StubServer::serve(|_: &str, _: &str| {
            StubReply::text(200, "xx component---src-pages-captcha yy")
        })
        .await;
        let client = reqwest::Client::new();
        let err = startpage_search_with_base(
            &client,
            "q",
            None,
            &format!("{}/", stub.base()),
            &format!("{}/sp/search", stub.base()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
        assert_eq!(
            stub.hits("/sp/search"),
            1,
            "homepage challenge degrades to the GET fallback"
        );
    }

    #[tokio::test]
    async fn header_order_matches_chrome_navigation_for_homepage_and_search_post() {
        // #58/#59: the homepage GET (`sec-fetch-site: none`, no Referer --
        // it is the chain's real first navigation) and the search POST
        // (`sec-fetch-site: same-origin`, Referer/Origin) must both match
        // Chrome's navigation header order, and both must carry the same
        // profile (#59 chain-reuse).
        let (stub, captured) = StubServer::serve_capturing_headers_startpage().await;
        let client = reqwest::Client::new();
        let rows = startpage_search_with_base(
            &client,
            "q",
            None,
            &format!("{}/", stub.base()),
            &format!("{}/sp/search", stub.base()),
        )
        .await
        .expect("stub always replies 200");
        assert_eq!(rows.len(), 0, "empty stub search page yields no rows");

        let requests = captured.lock().expect("captured");
        assert_eq!(requests.len(), 2, "homepage GET + one search POST");

        let expected_homepage: Vec<&str> = vec![
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
            // No Referer: the homepage GET is the chain's first request.
            "host",
        ];
        let expected_search_post: Vec<&str> = vec![
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
            "referer",
            "origin",
            "content-type",
            "host",
            "content-length",
        ];
        let homepage_names = super::super::test_support::header_names(&requests[0]);
        assert_eq!(
            homepage_names, expected_homepage,
            "homepage GET header order must match Chrome navigation order"
        );
        let search_post_names = super::super::test_support::header_names(&requests[1]);
        assert_eq!(
            search_post_names, expected_search_post,
            "search POST header order must match Chrome navigation order"
        );

        let header_value = |lines: &[String], name: &str| -> String {
            lines
                .iter()
                .find_map(|line| {
                    let (n, value) = line.split_once(':')?;
                    if n.trim().eq_ignore_ascii_case(name) {
                        Some(value.trim().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("{name} present"))
        };
        assert_eq!(header_value(&requests[0], "sec-fetch-site"), "none");
        assert_eq!(header_value(&requests[1], "sec-fetch-site"), "same-origin");
        assert_eq!(
            header_value(&requests[0], "user-agent"),
            header_value(&requests[1], "user-agent"),
            "homepage and search POST must reuse one profile"
        );
    }
}
