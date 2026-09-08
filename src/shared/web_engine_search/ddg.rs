//! DuckDuckGo leg of the fetch-only web search engine.
//!
//! Form POST plus `s`/`vqd` continuation re-POSTs over plain HTTP, parsed
//! with regex into raw hits for the agent-loop Queries. Selector drift
//! and bot-wall changes land here.

use std::collections::HashSet;

use regex::Regex;

use super::fanout::map_transport_error;
use crate::shared::types::search::{
    Recency, SearchProvider, SearchProviderError, SearchResult, DDG_KL_DEFAULT, MAX_NUM_RESULTS,
};
use crate::shared::web_engine_search::decode::{
    collapse_whitespace, decode_entities, decode_html_text, percent_decode, percent_encode,
    strip_tags,
};
use crate::shared::web_engine_search::{apply_browser_headers, leg_timeout};

/// DuckDuckGo no-JS frontend (Omp `DUCKDUCKGO_HTML_URL`). Tests override via
/// `ddg_search_with_base`; never the Instant Answer API (Omp tried and
/// discarded it: Wikipedia/Wolfram-style topics only).
pub const DDG_HTML_URL: &str = "https://html.duckduckgo.com/html/";

/// DDG Referer header sent with form POSTs.
pub const DDG_REFERER: &str = "https://html.duckduckgo.com/";

/// DDG bot-wall detector (Omp `isAnomalyResponse`): the `anomaly-modal` or
/// `anomaly.js` literals in the body. Status varies 200-202, so the body is
/// the signal. Matches even on status 200 (providers soft-block).
pub fn is_ddg_anomaly(body: &str) -> bool {
    body.contains("anomaly-modal") || body.contains("anomaly.js")
}

/// Map a finished DDG HTTP exchange to rows or a typed leg error: anomaly
/// body -> `Challenge` (429) even on status 200 (providers soft-block);
/// non-2xx without markers -> `Upstream` (503). Empty with no marker is
/// `Ok(vec![])` (zero results is not an error).
pub(crate) fn map_ddg_response(
    status: u16,
    body: &str,
    query: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    if is_ddg_anomaly(body) {
        return Err(SearchProviderError::Challenge {
            provider: SearchProvider::DuckDuckGo,
            detail: "DuckDuckGo blocked the request with a bot-detection challenge. Retry via an accredited provider or later.".to_string(),
        });
    }
    if !(200..300).contains(&status) {
        return Err(SearchProviderError::Upstream {
            provider: SearchProvider::DuckDuckGo,
            detail: format!("DuckDuckGo HTML error ({status})"),
        });
    }
    Ok(parse_ddg_html(body, query))
}

/// Strip `before:`/`after:` date operators from a raw Query (Omp
/// `DDG_QUERY_SYNTAX` without dates: DDG does not parse them; the time filter
/// travels via `df` instead). Collapses leftover whitespace.
fn format_ddg_query(raw: &str) -> String {
    let mut kept = Vec::new();
    for token in raw.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if lower.starts_with("before:") || lower.starts_with("after:") {
            continue;
        }
        kept.push(token);
    }
    kept.join(" ")
}

/// Build the DDG page-1 form (Omp `createDuckDuckGoForm`): `q` =
/// `format_ddg_query(query)`, `kl` = `DDG_KL_DEFAULT` (full `DDG_KL_CODES` /
/// `DDG_LOCALE_ALIASES` table is a follow-up), `df` = recency param when set,
/// `b` = `""` (imitates a real browser form submission).
fn create_ddg_form(query: &str, recency: Option<Recency>) -> Vec<(String, String)> {
    let mut form = vec![
        ("q".to_string(), format_ddg_query(query)),
        ("kl".to_string(), DDG_KL_DEFAULT.to_string()),
        ("b".to_string(), String::new()),
    ];
    if let Some(recency) = recency {
        form.push(("df".to_string(), recency.param().to_string()));
    }
    form
}

/// One DDG POST: form body + Referer + desktop headers, per-request timeout
/// ceiling (`LEG_TIMEOUT_SECS` in production, 1 s under `cfg(test)` so the
/// slow-stub timeout test runs in milliseconds). Returns status + body;
/// transport errors map to `Timeout` (504, `is_timeout`) or `Upstream` (503).
async fn ddg_post(
    client: &reqwest::Client,
    url: &str,
    form: &[(String, String)],
    query: &str,
) -> Result<(u16, String), SearchProviderError> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let response = apply_browser_headers(
        client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Referer", DDG_REFERER)
            .body(body)
            .timeout(leg_timeout()),
    )
    .send()
    .await
    .map_err(|err| {
        map_transport_error(
            SearchProvider::DuckDuckGo,
            query,
            err.is_timeout(),
            format!("DuckDuckGo request failed: {err}"),
        )
    })?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|err| {
        map_transport_error(
            SearchProvider::DuckDuckGo,
            query,
            err.is_timeout(),
            format!("DuckDuckGo body read failed: {err}"),
        )
    })?;
    Ok((status, text))
}

/// DDG leg: page-1 POST + `s`+`vqd` continuation re-POSTs (verbatim input
/// set) until `MAX_NUM_RESULTS` rows, no continuation form, or `s` stops
/// advancing. Exact-URL seen dedup across pages; global rank order.
pub(crate) async fn ddg_search(
    client: &reqwest::Client,
    query: &str,
    recency: Option<Recency>,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    let mut rows: Vec<SearchResult> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut form = create_ddg_form(query, recency);
    let mut last_s: Option<String> = None;
    loop {
        let (status, body) = ddg_post(client, base, &form, query).await?;
        let page = map_ddg_response(status, &body, query)?;
        for mut row in page {
            if !seen.insert(row.url.clone()) {
                continue;
            }
            row.rank = rows.len();
            rows.push(row);
            if rows.len() >= MAX_NUM_RESULTS {
                return Ok(rows);
            }
        }
        let Some(next) = parse_continuation_form(&body) else {
            return Ok(rows);
        };
        let next_s = next.iter().find(|(k, _)| k == "s").cloned().map(|(_, v)| v);
        if next_s.is_some() && next_s == last_s {
            return Ok(rows);
        }
        last_s = next_s;
        form = next;
    }
}

/// Best-effort timestamp from a DDG result block (Omp `extractPublishedDate`);
/// `None` when absent. Only DDG fills `published_date`.
fn extract_published_date(block: &str) -> Option<String> {
    let lower = block.to_ascii_lowercase();
    for marker in ["result__age", "result-age", "published", "datetime"] {
        if let Some(pos) = lower.find(marker) {
            let mut end = (pos + 320).min(block.len());
            while !block.is_char_boundary(end) {
                end -= 1;
            }
            let window = &block[pos..end];
            let text = collapse_whitespace(&decode_entities(&strip_tags(window)));
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// Unwrap a DDG result href (Omp `unwrapResultUrl`): click-wrapper
/// `//duckduckgo.com/l/?uddg=<encoded>` (decode `uddg` and use it),
/// protocol-relative `//...` (resolve to `https:`), rare absolute URL (as-is).
/// Non-http(s) after unwrap returns `None` (row skipped).
pub fn unwrap_ddg_url(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains("duckduckgo.com/l/") {
        if let Some(qpos) = trimmed.find('?') {
            for pair in trimmed[qpos + 1..].split('&') {
                let (key, value) = match pair.find('=') {
                    Some(eq) => (&pair[..eq], &pair[eq + 1..]),
                    None => continue,
                };
                if key == "uddg" {
                    let decoded = percent_decode(value);
                    if decoded.starts_with("http://") || decoded.starts_with("https://") {
                        return Some(decoded);
                    }
                    return None;
                }
            }
        }
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("//") {
        let candidate = format!("https://{rest}");
        if candidate.starts_with("https://") {
            return Some(candidate);
        }
        return None;
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(trimmed.to_string());
    }
    None
}

/// Parse DDG HTML (pure; 100% regex, no DOM -- Omp `parseHtmlResults` via
/// `blockRe`/`titleRe`/`snippetRe`): iterate `<div class="result ...">`
/// blocks; title from `<a class="result__a" href>` in either attribute order;
/// snippet from the `result__snippet` element (`a|div|span`, else `""`). Skips
/// rows with empty title or non-http(s) URL. `rank` is document order.
pub fn parse_ddg_html(html: &str, query: &str) -> Vec<SearchResult> {
    static OPEN_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static TITLE_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static SNIPPET_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let open_re = OPEN_RE.get_or_init(|| {
        Regex::new(r#"<div[^>]*class="[^"]*\bresult\b[^"]*"[^>]*>"#)
            .expect("static DDG block regex")
    });
    let title_re = TITLE_RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<a[^>]*class="[^"]*\bresult__a\b[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>|(?s)<a[^>]*href="([^"]+)"[^>]*class="[^"]*\bresult__a\b[^"]*"[^>]*>(.*?)</a>"#,
        )
        .expect("static DDG title regex")
    });
    let snippet_re = SNIPPET_RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<(?:a|div|span)[^>]*class="[^"]*\bresult__snippet\b[^"]*"[^>]*>(.*?)</(?:a|div|span)>"#,
        )
        .expect("static DDG snippet regex")
    });
    let opens: Vec<usize> = open_re.find_iter(html).map(|m| m.start()).collect();
    let mut rows = Vec::new();
    for (idx, start) in opens.iter().enumerate() {
        let slice_end = opens.get(idx + 1).copied().unwrap_or(html.len());
        let block = html[*start..slice_end.min(html.len())].to_string();
        let Some(title_cap) = title_re.captures(&block) else {
            continue;
        };
        // Either attribute order may match: (href, title) is (1, 2) or (3, 4).
        let (href, title_raw) = if title_cap.get(1).is_some() {
            (&title_cap[1], &title_cap[2])
        } else {
            (&title_cap[3], &title_cap[4])
        };
        let href = href.to_string();
        let title = decode_html_text(title_raw);
        if title.is_empty() {
            continue;
        }
        let Some(url) = unwrap_ddg_url(&href) else {
            continue;
        };
        let snippet = snippet_re
            .captures(&block)
            .map(|c| decode_html_text(&c[1]))
            .unwrap_or_default();
        let published_date = extract_published_date(&block);
        rows.push(SearchResult {
            title,
            url,
            snippet,
            provider: SearchProvider::DuckDuckGo,
            query: query.to_string(),
            rank: rows.len(),
            published_date,
        });
    }
    rows
}

/// Parse the DDG next-page form (Omp `parseContinuationForm`): all `<input>`s
/// of the next-page form, verbatim; requires `s` + `vqd`, else `None` (stop).
pub fn parse_continuation_form(html: &str) -> Option<Vec<(String, String)>> {
    static FORM_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static INPUT_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let form_re = FORM_RE
        .get_or_init(|| Regex::new(r"(?s)<form[^>]*>(.*?)</form>").expect("static form regex"));
    let input_re = INPUT_RE.get_or_init(|| {
        Regex::new(
            r#"<input[^>]*name="([^"]+)"[^>]*value="([^"]*)"[^>]*>|<input[^>]*value="([^"]*)"[^>]*name="([^"]+)"[^>]*>"#,
        )
        .expect("static input regex")
    });
    for form in form_re.captures_iter(html).map(|c| c[1].to_string()) {
        let mut inputs = Vec::new();
        for cap in input_re.captures_iter(&form) {
            let pair = cap
                .get(1)
                .zip(cap.get(2))
                .or_else(|| cap.get(4).zip(cap.get(3)));
            if let Some((name, value)) = pair {
                inputs.push((
                    decode_entities(name.as_str()),
                    decode_entities(value.as_str()),
                ));
            }
        }
        let has_s = inputs.iter().any(|(k, v)| k == "s" && !v.is_empty());
        let has_vqd = inputs.iter().any(|(k, v)| k == "vqd" && !v.is_empty());
        if has_s && has_vqd {
            return Some(inputs);
        }
    }
    None
}

#[doc(hidden)]
pub async fn ddg_search_with_base(
    client: &reqwest::Client,
    query: &str,
    recency: Option<Recency>,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    ddg_search(client, query, recency, base).await
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{StubReply, StubServer};
    use super::*;
    use crate::shared::web_engine_search::startpage::is_startpage_challenge;

    #[test]
    fn challenge_detectors() {
        // DDG anomaly literals (Omp `isAnomalyResponse`); status varies
        // 200-202, so the body is the signal.
        assert!(is_ddg_anomaly(r#"<div id="anomaly-modal"></div>"#));
        assert!(is_ddg_anomaly(r#"<script src="/anomaly.js"></script>"#));
        assert!(!is_ddg_anomaly("<div class=\"result\">ok</div>"));
        // Startpage challenge (Omp `isChallengeResponse`): redirect forms or
        // the captcha-page marker ...
        assert!(is_startpage_challenge(
            "",
            "https://www.startpage.com/en/errors/captcha"
        ));
        assert!(is_startpage_challenge(
            "",
            "https://www.startpage.com/sp/captcha"
        ));
        assert!(is_startpage_challenge(
            "",
            "https://www.startpage.com/sp/captcha-block?bc=PY&query=obscura+browser"
        ));
        assert!(is_startpage_challenge(
            "xx component---src-pages-captcha yy",
            "https://x/"
        ));
        // ... but a bare "captcha" substring MUST NOT trigger (captcha-topic
        // query snippets would false-positive).
        assert!(!is_startpage_challenge(
            "how do captchas work? explained",
            "https://x/"
        ));
        assert!(!is_startpage_challenge(
            "<div class=\"result\">ok</div>",
            "https://x/"
        ));
    }

    #[test]
    fn ddg_parse_blocks_titles_snippets() {
        let html = r#"
<div class="result results_links results_links_deep web-result ">
  <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffirst&amp;rut=x">First <b>Result</b></a></h2>
  <a class="result__snippet" href="https://example.com/first">first snippet &amp; more</a>
</div>
<div class="result results_links results_links_deep web-result ">
  <h2 class="result__title"><a rel="nofollow" class="result__a" href="https://example.com/second">Second Title</a></h2>
  <div class="result__snippet">second <b>snippet</b> here</div>
</div>
<div class="result results_links results_links_deep web-result ">
  <h2 class="result__title"><a rel="nofollow" class="result__a" href="https://example.com/third">Third</a></h2>
  <span class="result__snippet">third &#x27;quoted&#x27; text</span>
</div>
<div class="result results_links results_links_deep web-result ">
  <h2 class="result__title"><a rel="nofollow" href="https://example.com/reversed" class="result__a">Reversed Attrs</a></h2>
  <div class="result__snippet">reversed snippet</div>
</div>
"#;
        let rows = parse_ddg_html(html, "probe query");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].title, "First Result");
        assert_eq!(rows[0].url, "https://example.com/first");
        assert_eq!(rows[0].snippet, "first snippet & more");
        assert_eq!(rows[0].rank, 0);
        assert_eq!(rows[0].query, "probe query");
        assert_eq!(rows[0].provider, SearchProvider::DuckDuckGo);
        assert_eq!(rows[1].url, "https://example.com/second");
        assert_eq!(rows[1].snippet, "second snippet here");
        assert_eq!(rows[2].snippet, "third 'quoted' text");
        assert_eq!(rows[3].title, "Reversed Attrs");
        assert_eq!(rows[3].url, "https://example.com/reversed");
    }

    #[test]
    fn ddg_decode_bare_ampersand_and_unicode() {
        // Bare `&` with no `;` anywhere after it must survive literally
        // (real snippets contain "AT&T"-style text); regression: the old
        // `find(';').unwrap_or(MAX).min(32)` guard sliced `rest[1..32]` and
        // panicked on short tails.
        // Non-ASCII text must round-trip intact (byte-wise iteration turned
        // "S\u{e3}o" into mojibake).
        let html = "<div class=\"result\">\n<a class=\"result__a\" href=\"https://example.com/bare\">Bare</a>\n<div class=\"result__snippet\">AT&T deals in S\u{e3}o Paulo caf\u{e9}</div>\n</div>";
        let rows = parse_ddg_html(html, "q");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].snippet, "AT&T deals in S\u{e3}o Paulo caf\u{e9}");
    }

    #[test]
    fn ddg_unwrap_percent_encoded_unicode() {
        // `%C3%A9` is UTF-8 for `\u{e9}`: the unwrapped URL must carry the
        // decoded character, not one `char` per byte (mojibake).
        assert_eq!(
            unwrap_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fcaf%C3%A9&rut=x"),
            Some("https://example.com/caf\u{e9}".to_string())
        );
    }

    #[test]
    fn ddg_redirect_unwrap() {
        assert_eq!(
            unwrap_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Ffirst&rut=x"),
            Some("https://example.com/first".to_string())
        );
        assert_eq!(
            unwrap_ddg_url("//example.com/proto-relative"),
            Some("https://example.com/proto-relative".to_string())
        );
        assert_eq!(
            unwrap_ddg_url("https://example.com/absolute"),
            Some("https://example.com/absolute".to_string())
        );
        assert_eq!(unwrap_ddg_url("javascript:void(0)"), None);
        assert_eq!(unwrap_ddg_url("ftp://example.com/x"), None);
    }

    #[test]
    fn ddg_continuation_requires_s_and_vqd() {
        let with_both = r#"<form action="/html/" method="post">
<input type="hidden" name="s" value="42"/>
<input type="hidden" name="vqd" value="abc123"/>
<input type="hidden" name="q" value="probe"/>
</form>"#;
        let inputs = parse_continuation_form(with_both).expect("s+vqd present");
        assert!(inputs.iter().any(|(k, v)| k == "s" && v == "42"));
        assert!(inputs.iter().any(|(k, v)| k == "vqd" && v == "abc123"));
        let missing_vqd = r#"<form action="/html/" method="post">
<input type="hidden" name="s" value="42"/>
<input type="hidden" name="q" value="probe"/>
</form>"#;
        assert_eq!(parse_continuation_form(missing_vqd), None);
        let missing_s = r#"<form action="/html/" method="post">
<input type="hidden" name="vqd" value="abc123"/>
</form>"#;
        assert_eq!(parse_continuation_form(missing_s), None);
    }

    #[tokio::test]
    async fn ddg_clamp_20_with_pagination() {
        // Body-aware stub drives the production `ddg_search_with_base` loop
        // end to end: page 1 serves 12 rows (0..12) + a continuation form
        // when the POST body lacks `s=`, page 2 serves 12 rows (0..12 again,
        // so cross-page exact-URL dedup collapses them) with no continuation
        // form. Ranks continue globally; output clamps to `MAX_NUM_RESULTS`.
        fn ddg_page(rows: std::ops::Range<usize>, continuation: bool) -> String {
            let mut html = String::from("<html><body>");
            for i in rows {
                html.push_str(&format!(
                    r#"<div class="result"><h2 class="result__title"><a class="result__a" href="https://example.com/p{i}">T{i}</a></h2><div class="result__snippet">s{i}</div></div>"#
                ));
            }
            if continuation {
                html.push_str(
                    r#"<form action="/html/" method="post"><input type="hidden" name="s" value="12"/><input type="hidden" name="vqd" value="tok"/><input type="hidden" name="q" value="q"/></form>"#,
                );
            }
            html.push_str("</body></html>");
            html
        }
        let stub = StubServer::serve(|_: &str, body: &str| {
            if body.contains("s=12") {
                StubReply::text(200, &ddg_page(0..12, false))
            } else {
                StubReply::text(200, &ddg_page(0..12, true))
            }
        })
        .await;
        let client = reqwest::Client::new();
        let rows = ddg_search_with_base(&client, "q", None, &format!("{}/html/", stub.base()))
            .await
            .expect("paginated stub merges");
        assert!(rows.len() <= MAX_NUM_RESULTS);
        assert_eq!(rows.len(), 12);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.rank, i);
        }
        assert_eq!(stub.hits("/html/"), 2, "both stub pages fetched");
    }

    #[test]
    fn ddg_published_date_extracted() {
        let html = r#"<div class="result">
<a class="result__a" href="https://example.com/d">Dated</a>
<span class="result__age">3 days ago</span>
</div>"#;
        let rows = parse_ddg_html(html, "q");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].published_date.is_some());
    }

    #[test]
    fn published_date_non_ascii_window_is_char_boundary_safe() {
        // Regression: the 320-byte scan window end landed inside a multi-byte
        // char and `&block[pos..end]` panicked (`byte index is not a char
        // boundary`). Marker at byte 0, 308 ASCII bytes, then `é` straddling
        // byte 320: the old slice end panicked, the clamped one must not.
        let block = format!("result__age{}é tail text", "a".repeat(308));
        let date = extract_published_date(&block);
        assert!(date.is_some(), "date text still extracted past clamp");
    }

    #[test]
    fn anomaly_body_maps_429_on_status_200() {
        let err = map_ddg_response(200, "<div id=\"anomaly-modal\"></div>", "q").unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(err.code(), "challenge");
        assert_eq!(err.provider(), Some(SearchProvider::DuckDuckGo));
    }

    #[tokio::test]
    async fn no_retry_on_challenge() {
        let stub = StubServer::serve(|path: &str, _body: &str| {
            if path.starts_with("/html") {
                StubReply::text(200, "<div id=\"anomaly-modal\"></div>")
            } else {
                StubReply::text(404, "nope")
            }
        })
        .await;
        let client = reqwest::Client::new();
        let err = ddg_search_with_base(&client, "q", None, &format!("{}/html/", stub.base()))
            .await
            .unwrap_err();
        assert_eq!(err.http_status(), 429);
        assert_eq!(
            stub.hits("/html/"),
            1,
            "challenge costs exactly one HTTP hit"
        );
    }

    #[tokio::test]
    async fn stub_500_maps_503() {
        let stub = StubServer::serve(|_: &str, _: &str| StubReply::text(500, "boom")).await;
        let client = reqwest::Client::new();
        let err = ddg_search_with_base(&client, "q", None, &format!("{}/html/", stub.base()))
            .await
            .unwrap_err();
        assert_eq!(err.http_status(), 503);
    }

    #[tokio::test]
    async fn hanging_leg_maps_504() {
        // Slow stub against the real leg entry point: `ddg_search_with_base`
        // carries the per-request `leg_timeout()` (1 s under `cfg(test)`),
        // so a hanging leg surfaces as `Timeout` (504). Timeout beats
        // Challenge. Production keeps `LEG_TIMEOUT_SECS`.
        let stub = StubServer::serve_slow(std::time::Duration::from_secs(5)).await;
        let client = reqwest::Client::new();
        let err = ddg_search_with_base(&client, "q", None, &format!("{}/html/", stub.base()))
            .await
            .expect_err("slow stub must time out at leg level");
        assert_eq!(err.http_status(), 504);
        assert_eq!(err.code(), "timeout");
    }

    #[test]
    fn ddg_query_date_ops_stripped() {
        assert_eq!(
            format_ddg_query("obscura before:2024-01-01 browser"),
            "obscura browser"
        );
        assert_eq!(format_ddg_query("after:2023-01-01 x"), "x");
        assert_eq!(format_ddg_query("plain query"), "plain query");
    }
}
