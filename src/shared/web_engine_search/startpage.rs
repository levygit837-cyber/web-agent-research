//! Startpage leg of the fetch-only web search engine.
//!
//! Homepage token fetch, verbatim form POST with direct-GET fallback,
//! DOM parse into raw hits for the agent-loop Queries. Selector drift
//! and bot-wall changes land here.

use std::collections::HashSet;

use scraper::{Html, Selector};

use super::fanout::map_transport_error;
use crate::shared::types::search::{
    Recency, SearchProvider, SearchProviderError, SearchResult, MAX_NUM_RESULTS,
};
use crate::shared::web_engine_search::decode::{collapse_whitespace, percent_encode};
use crate::shared::web_engine_search::{apply_browser_headers, leg_timeout};

/// Startpage homepage for anti-bot `sc` token extraction (Omp `fetchFormInputs`).
pub const STARTPAGE_HOME_URL: &str = "https://www.startpage.com/";

/// Startpage search POST target (Omp `STARTPAGE_SEARCH_URL`).
pub const STARTPAGE_SEARCH_URL: &str = "https://www.startpage.com/sp/search";

/// Startpage CAPTCHA detector (Omp `isChallengeResponse`): redirect to
/// `/en/errors/` or `/sp/captcha` (the live `/sp/captcha-block` redirect form
/// observed 2026-09-08 is subsumed by the `/sp/captcha` prefix), or the
/// `component---src-pages-captcha` marker in the body. A bare `"captcha"`
/// substring MUST NOT trigger (captcha-topic query snippets would
/// false-positive).
pub fn is_startpage_challenge(body: &str, final_url: &str) -> bool {
    final_url.contains("/en/errors/")
        || final_url.contains("/sp/captcha")
        || body.contains("component---src-pages-captcha")
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
            detail: "Startpage blocked the request with a CAPTCHA challenge. Retry via an accredited provider or later.".to_string(),
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

/// Homepage fetch outcome: token inputs or a silent miss (any failure,
/// non-OK status, homepage challenge wall, markup drift) that degrades to
/// the direct-GET fallback. The fallback body re-runs challenge detection,
/// so a live wall still surfaces as `Challenge` (429).
enum FormFetch {
    Inputs(Vec<(String, String)>),
    Miss,
}

/// Startpage form fetch (Omp `fetchFormInputs`, best effort): GET the homepage
/// and extract hidden inputs incl. the anti-bot `sc` token. Failures, non-OK
/// status, a homepage challenge, and markup drift (no `sc`) are all
/// `FormFetch::Miss` and degrade to the direct-GET fallback per SPEC §7
/// step 3.
async fn fetch_startpage_form_inputs(
    client: &reqwest::Client,
    home_url: &str,
    query: &str,
) -> FormFetch {
    let _ = query;
    let Ok(response) = apply_browser_headers(client.get(home_url).timeout(leg_timeout()))
        .send()
        .await
    else {
        return FormFetch::Miss;
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
/// (`+with_date` when recency is set).
async fn startpage_post(
    client: &reqwest::Client,
    url: &str,
    inputs: &[(String, String)],
    query: &str,
    recency: Option<Recency>,
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
    let response = apply_browser_headers(
        client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Referer", STARTPAGE_HOME_URL)
            .body(body)
            .timeout(leg_timeout()),
    )
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
async fn startpage_get(
    client: &reqwest::Client,
    url: &str,
    query: &str,
    recency: Option<Recency>,
) -> Result<(u16, String, String), SearchProviderError> {
    let mut request = apply_browser_headers(
        client
            .get(url)
            .query(&[("query", query)])
            .timeout(leg_timeout()),
    );
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
    match fetch_startpage_form_inputs(client, home_url, query).await {
        FormFetch::Inputs(inputs) => {
            let (status, body, final_url) =
                startpage_post(client, search_url, &inputs, query, recency).await?;
            return map_startpage_response(status, &body, &final_url, query);
        }
        FormFetch::Miss => {}
    }
    let (status, body, final_url) = startpage_get(client, search_url, query, recency).await?;
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
    use crate::shared::web_engine_search::test_support::{StubReply, StubServer};

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
        // fallback per SPEC §7 step 3, and the fallback wall body re-triggers
        // challenge detection, so the leg still reports Challenge (429).
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
}
