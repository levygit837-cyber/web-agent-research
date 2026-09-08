//! Fetch-only search Module: DDG + Startpage tools with parallel fan-out.
//!
//! Deep-module layout: a small Interface (`search_multi`, `search_tool_schema`,
//! `dedup_key`, `merge_sources`, two pure HTML parsers, two pure challenge
//! detectors) over a large Implementation (DDG form + continuation handling,
//! Startpage token/GET-fallback handling, per-provider error mapping, soft/hard
//! fan-out). Selector drift and bot-wall changes land in this one file.
//!
//! Rules: concrete functions, no traits, one shared `&reqwest::Client` passed
//! in. Search results are ungrounded candidates, never Evidence.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use regex::Regex;
use scraper::{Html, Selector};
use serde_json::{json, Value};

use crate::shared::types::search::{
    MergedResult, Recency, SearchInput, SearchOutput, SearchProvider, SearchProviderError,
    SearchResult, SearchStats, DDG_KL_DEFAULT, HARD_DEADLINE_SECS, MAX_NUM_RESULTS,
    SOFT_DEADLINE_SECS,
};

/// Agent-loop tool name.
pub const SEARCH_TOOL_NAME: &str = "search";

/// DuckDuckGo no-JS frontend (Omp `DUCKDUCKGO_HTML_URL`). Tests override via
/// `ddg_search_with_base`; never the Instant Answer API (Omp tried and
/// discarded it: Wikipedia/Wolfram-style topics only).
pub const DDG_HTML_URL: &str = "https://html.duckduckgo.com/html/";
/// DDG Referer header sent with form POSTs.
pub const DDG_REFERER: &str = "https://html.duckduckgo.com/";
/// Startpage homepage for anti-bot `sc` token extraction (Omp `fetchFormInputs`).
pub const STARTPAGE_HOME_URL: &str = "https://www.startpage.com/";
/// Normalised identity of a result URL used for dedup: lowercase host minus
/// one leading `www.` + path minus one trailing `/` (only when len > 1) +
/// `?query` preserved verbatim + no fragment. Literal port of Omp `dedupKey`.
/// Total: never panics; parse failure returns the trimmed raw input.
pub fn dedup_key(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    let Some(host) = parsed.host_str() else {
        return trimmed.to_string();
    };
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let mut key = String::with_capacity(trimmed.len());
    key.push_str(host);
    let path = parsed.path();
    if path.len() > 1 {
        key.push_str(path.strip_suffix('/').unwrap_or(path));
    } else if path == "/" && parsed.query().is_none() && trimmed.ends_with('/') {
        // Root path: keep bare host ("/a/" vs "/a" rule only applies to len > 1).
    } else if path != "/" {
        key.push_str(path);
    }
    if let Some(query) = parsed.query() {
        key.push('?');
        key.push_str(query);
    }
    key
}

/// Fold raw hits into deduped groups. Port of Omp `mergeSources` + consensus
/// sort, with the Query dimension added for multi-query fan-out: iterate rows
/// grouped by leg in engine priority order (Startpage legs first in Query
/// order, then DDG legs in Query order), never arrival order. Per group the
/// best rank adopts title + display URL, longest snippet wins, published date
/// is first-win. Sort: provider count desc, best rank asc, display URL asc.
/// `queries` is the validated fan-out input order: legs key by
/// `(priority, query_index)` so duplicate query strings never collapse.
pub fn merge_sources(results: Vec<SearchResult>) -> Vec<MergedResult> {
    merge_sources_in_order(results, &[])
}

/// Order-aware merge core: `queries[i]` is the input string of leg index `i`.
/// Rows whose `query` is absent from `queries` (ad-hoc unit fixtures) fall
/// back to first-seen index order, preserving the old deterministic output.
fn merge_sources_in_order(results: Vec<SearchResult>, queries: &[String]) -> Vec<MergedResult> {
    let mut legs: HashMap<(usize, usize), Vec<SearchResult>> = HashMap::new();
    let mut fallback_next: usize = queries.len();
    let mut fallback_index: HashMap<String, usize> = HashMap::new();
    for row in results {
        let query_index = queries
            .iter()
            .position(|q| *q == row.query)
            .unwrap_or_else(|| {
                *fallback_index.entry(row.query.clone()).or_insert_with(|| {
                    let index = fallback_next;
                    fallback_next += 1;
                    index
                })
            });
        legs.entry((row.provider.priority(), query_index))
            .or_default()
            .push(row);
    }
    let mut leg_keys: Vec<(usize, usize)> = legs.keys().cloned().collect();
    leg_keys.sort();
    let mut groups: HashMap<String, MergedResult> = HashMap::new();
    for leg in leg_keys {
        let rows = legs.remove(&leg).unwrap_or_default();
        for row in rows {
            let key = dedup_key(&row.url);
            if key.trim().is_empty() {
                continue;
            }
            match groups.get_mut(&key) {
                None => {
                    groups.insert(
                        key.clone(),
                        MergedResult {
                            title: row.title.clone(),
                            canonical_url: key,
                            display_url: row.url.clone(),
                            snippet: row.snippet.clone(),
                            providers: vec![row.provider],
                            queries: vec![row.query.clone()],
                            best_rank: row.rank,
                            hit_count: 1,
                            published_date: row.published_date.clone(),
                        },
                    );
                }
                Some(group) => {
                    if !group.providers.contains(&row.provider) {
                        group.providers.push(row.provider);
                        group.providers.sort_by_key(|p| p.priority());
                    }
                    if !group.queries.contains(&row.query) {
                        group.queries.push(row.query.clone());
                        group.queries.sort();
                    }
                    if row.rank < group.best_rank {
                        group.best_rank = row.rank;
                        group.title.clone_from(&row.title);
                        group.display_url.clone_from(&row.url);
                    }
                    if !row.snippet.is_empty() && row.snippet.len() > group.snippet.len() {
                        group.snippet.clone_from(&row.snippet);
                    }
                    if group.published_date.is_none() {
                        group.published_date.clone_from(&row.published_date);
                    }
                    group.hit_count += 1;
                }
            }
        }
    }
    let mut out: Vec<MergedResult> = groups.into_values().collect();
    out.sort_by(|a, b| {
        b.providers
            .len()
            .cmp(&a.providers.len())
            .then_with(|| a.best_rank.cmp(&b.best_rank))
            .then_with(|| a.display_url.cmp(&b.display_url))
    });
    out
}

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
fn map_ddg_response(
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

/// Map a finished Startpage HTTP exchange to rows or a typed leg error:
/// challenge signals -> `Challenge` (429); non-2xx without markers ->
/// `Upstream` (503); otherwise parse (possibly empty).
fn map_startpage_response(
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

/// Build the typed leg-timeout error. Timeout beats Challenge: callers map
/// transport timeouts (or fan-out deadline cuts) here before inspecting any
/// body, so a slow challenge page still reports `Timeout` (504).
fn map_timeout(provider: SearchProvider, query: &str) -> SearchProviderError {
    SearchProviderError::Timeout {
        provider,
        query: query.to_string(),
    }
}

/// Map a `reqwest` transport error for one leg: timeouts -> `Timeout` (504),
/// everything else -> `Upstream` (503). Pure over the error predicates so
/// unit tests pin the mapping without network.
fn map_transport_error(
    provider: SearchProvider,
    query: &str,
    is_timeout: bool,
    detail: String,
) -> SearchProviderError {
    if is_timeout {
        map_timeout(provider, query)
    } else {
        SearchProviderError::Upstream { provider, detail }
    }
}
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

/// Format leg failures as `"id: msg; id: msg"` (Omp
/// `formatSearchProviderFailures`).
pub fn all_failed_message(failures: &[(String, String)]) -> String {
    failures
        .iter()
        .map(|(id, msg)| format!("{id}: {msg}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Deep-module entry: full fan-out (validate is the caller's job -- this
/// takes a validated `SearchInput`; run 2xN legs under soft/hard deadlines
/// -> merge -> truncate). Accepts the shared client, creates nothing.
/// Leg order is deterministic: per Query in input order, Startpage then DDG.
/// Never fails a whole batch on one leg: leg errors collect into
/// `output.errors`; only total-leg failure (`AllFailed`: merged empty AND
/// every leg failed) returns `Err`. Otherwise `Ok` -- including partial
/// success and empty results (zero results with no challenge marker is not an
/// error). Leg panics surface as `Upstream { detail: "leg panicked" }`.
pub async fn search_multi(
    client: &reqwest::Client,
    input: SearchInput,
) -> Result<SearchOutput, SearchProviderError> {
    search_multi_with_bases(
        client,
        input,
        DDG_HTML_URL,
        STARTPAGE_HOME_URL,
        STARTPAGE_SEARCH_URL,
    )
    .await
}

/// One settled fan-out leg: spawn index, provider, query, and outcome.
type SettledLeg = (
    usize,
    SearchProvider,
    String,
    Result<Vec<SearchResult>, SearchProviderError>,
);

/// Test override for the fan-out endpoints (local `TcpListener` stubs); the
/// production path above pins the real hosts.
#[doc(hidden)]
pub async fn search_multi_with_bases(
    client: &reqwest::Client,
    input: SearchInput,
    ddg_base: &str,
    sp_home: &str,
    sp_search: &str,
) -> Result<SearchOutput, SearchProviderError> {
    let queries = input.queries.clone();
    let recency = input.recency;
    let top_k = input.top_k;
    // Legs in deterministic order: per Query in input order, Startpage then DDG.
    // Each leg keeps its spawn index (query index + provider): JoinSet returns
    // legs in completion order, so results reconstruct by index, never by
    // (query, provider) — duplicate query strings no longer collapse, and a
    // panicked leg maps back to its slot via JoinError::id.
    let mut set = tokio::task::JoinSet::new();
    let leg_count = queries.len() * 2;
    let mut leg_ids: Vec<tokio::task::Id> = Vec::with_capacity(leg_count);
    let mut leg_slots: Vec<(usize, SearchProvider)> = Vec::with_capacity(leg_count);
    for (query_index, query) in queries.iter().enumerate() {
        for provider in [SearchProvider::Startpage, SearchProvider::DuckDuckGo] {
            let (client_owned, query_owned) = (client.clone(), query.clone());
            let (ddg, home, search) = (
                ddg_base.to_string(),
                sp_home.to_string(),
                sp_search.to_string(),
            );
            let slot = (query_index, provider);
            leg_slots.push(slot);
            leg_ids.push(
                set.spawn(async move {
                    let outcome: Result<Vec<SearchResult>, SearchProviderError> = match provider {
                        SearchProvider::Startpage => {
                            startpage_search(&client_owned, &query_owned, recency, &home, &search)
                                .await
                        }
                        SearchProvider::DuckDuckGo => {
                            ddg_search(&client_owned, &query_owned, recency, &ddg).await
                        }
                    };
                    (slot, query_owned, outcome)
                })
                .id(),
            );
        }
    }
    // Race three exits: all legs settle; soft deadline with >=1 success;
    // hard cap regardless. Stragglers abort; their legs report `Timeout`.
    let soft = Duration::from_secs(SOFT_DEADLINE_SECS);
    let hard = Duration::from_secs(HARD_DEADLINE_SECS);
    let mut settled: Vec<SettledLeg> = Vec::with_capacity(leg_count);
    let mut successes: usize = 0;
    let deadline = tokio::time::Instant::now() + hard;
    let mut soft_fired = false;
    loop {
        if settled.len() >= leg_count {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let slot = if !soft_fired {
            let elapsed = hard.saturating_sub(remaining);
            let to_soft = soft.saturating_sub(elapsed);
            tokio::select! {
                next = set.join_next() => next,
                () = tokio::time::sleep(to_soft) => {
                    soft_fired = true;
                    if successes >= 1 {
                        break;
                    }
                    continue;
                }
            }
        } else {
            tokio::select! {
                next = set.join_next() => next,
                () = tokio::time::sleep(remaining) => break,
            }
        };
        match slot {
            Some(Ok(((query_index, provider), query, outcome))) => {
                if outcome.is_ok() {
                    successes += 1;
                }
                settled.push((query_index, provider, query, outcome));
            }
            Some(Err(join)) => {
                // Leg task panicked: typed `Upstream` on its own slot, never
                // propagates. JoinError::id maps back to the spawned leg.
                let (query_index, provider) = leg_ids
                    .iter()
                    .position(|id| *id == join.id())
                    .map(|spawn_index| leg_slots[spawn_index])
                    .unwrap_or((0, SearchProvider::DuckDuckGo));
                let query = queries.get(query_index).cloned().unwrap_or_default();
                settled.push((
                    query_index,
                    provider,
                    query,
                    Err(SearchProviderError::Upstream {
                        provider,
                        detail: "leg panicked".to_string(),
                    }),
                ));
            }
            None => break,
        }
    }
    set.abort_all();
    // Legs cut by the deadlines report `Timeout`. Reconstruct leg order by
    // spawn index: settled legs keep results; missing slots (per Query in
    // input order, SP then DDG) time out. Duplicate query strings keep
    // separate slots.
    let mut by_leg: std::collections::HashMap<
        usize,
        Result<Vec<SearchResult>, SearchProviderError>,
    > = std::collections::HashMap::new();
    for (query_index, provider, _query, outcome) in settled {
        let slot = query_index * 2 + usize::from(provider != SearchProvider::Startpage);
        by_leg.insert(slot, outcome);
    }
    let mut raw_hits: Vec<SearchResult> = Vec::new();
    let mut errors: Vec<SearchProviderError> = Vec::new();
    for (query_index, query) in queries.iter().enumerate() {
        for provider in [SearchProvider::Startpage, SearchProvider::DuckDuckGo] {
            let slot = query_index * 2 + usize::from(provider != SearchProvider::Startpage);
            match by_leg.remove(&slot) {
                Some(Ok(rows)) => raw_hits.extend(rows),
                Some(Err(err)) => errors.push(err),
                None => errors.push(map_timeout(provider, query)),
            }
        }
    }
    let raw_count = raw_hits.len();
    let mut merged = merge_sources_in_order(raw_hits, &queries);
    let merged_count = merged.len();
    if merged.is_empty() && errors.len() == leg_count && leg_count > 0 {
        let failures: Vec<(String, String)> = errors
            .iter()
            .map(|err| {
                let id = err
                    .provider()
                    .map(|p| p.id().to_string())
                    .unwrap_or_else(|| "leg".to_string());
                (id, error_detail(err))
            })
            .collect();
        return Err(SearchProviderError::AllFailed {
            failures: all_failed_message(&failures),
        });
    }
    merged.truncate(top_k);
    Ok(SearchOutput {
        results: merged,
        errors,
        stats: SearchStats {
            queries: queries.len(),
            legs: leg_count,
            raw_hits: raw_count,
            merged: merged_count,
        },
    })
}

fn error_detail(err: &SearchProviderError) -> String {
    match err {
        SearchProviderError::EmptyQuery => "empty query".to_string(),
        SearchProviderError::TooManyQueries { got, max } => {
            format!("too many queries: {got} > {max}")
        }
        SearchProviderError::Challenge { detail, .. } => detail.clone(),
        SearchProviderError::Timeout { query, .. } => format!("timed out: {query}"),
        SearchProviderError::Upstream { detail, .. } => detail.clone(),
        SearchProviderError::AllFailed { failures } => failures.clone(),
    }
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

/// Agent-loop tool schema object (JSON Schema draft 2020-12,
/// `additionalProperties: false` everywhere). Pure, infallible. The agent
/// loop exposes exactly one tool named `search`: 1-8 queries, optional
/// top_k (1-30, default 15), optional recency enum.
/// Desktop navigation header profile (port of Omp
/// `buildBrowserNavigationHeaders` without the `HeaderGenerator`: static UA +
/// `Sec-CH-UA*` + `Sec-Fetch-*` + `Accept-Language`). Shared by every leg.
fn apply_browser_headers(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
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
fn leg_timeout() -> Duration {
    #[cfg(test)]
    return Duration::from_secs(1);
    #[cfg(not(test))]
    return Duration::from_secs(crate::shared::types::search::LEG_TIMEOUT_SECS);
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

fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// DDG leg: page-1 POST + `s`+`vqd` continuation re-POSTs (verbatim input
/// set) until `MAX_NUM_RESULTS` rows, no continuation form, or `s` stops
/// advancing. Exact-URL seen dedup across pages; global rank order.
async fn ddg_search(
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
async fn startpage_search(
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

pub fn search_tool_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "name": SEARCH_TOOL_NAME,
        "description": "Fetch-only web search over DuckDuckGo and Startpage. Takes 1-8 queries, fans out to both providers in parallel, dedups by URL key and returns consensus-ranked merged results. Returned snippets are ungrounded candidates, not Evidence: fetch a result URL before citing it.",
        "parameters": {
            "properties": {
                "queries": {
                    "type": "array",
                    "description": "Research queries for this Turn. 1-8 items, each 1-500 chars.",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": { "type": "string", "minLength": 1, "maxLength": 500 }
                },
                "top_k": {
                    "type": "integer",
                    "description": "Max merged results to return. Defaults to 15.",
                    "minimum": 1,
                    "maximum": 30,
                    "default": 15
                },
                "recency": {
                    "type": "string",
                    "description": "Optional freshness window, mapped per provider (DDG df / Startpage with_date). Omit for no time filter.",
                    "enum": ["day", "week", "month", "year"]
                }
            },
            "required": ["queries"],
            "additionalProperties": false
        }
    })
}

/// Startpage search POST target (Omp `STARTPAGE_SEARCH_URL`).
pub const STARTPAGE_SEARCH_URL: &str = "https://www.startpage.com/sp/search";

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

/// Decode HTML text: strip inline tags (Omp `decodeHtmlText` `<b>`
/// highlights), decode named + numeric entities, normalise whitespace.
fn decode_html_text(raw: &str) -> String {
    let no_tags = strip_tags(raw);
    decode_entities(&no_tags)
}

fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut inside = false;
    for ch in raw.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

fn decode_entities(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let semi = match after.find(';') {
            Some(pos) if pos <= 32 => pos,
            _ => {
                // No `;` within entity range: bare `&` (e.g. "AT&T
                // tails" or a truncated tail) stays literal.
                out.push('&');
                rest = after;
                continue;
            }
        };
        let entity = &after[..semi];
        let decoded = match entity {
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "quot" => Some("\"".to_string()),
            "apos" => Some("'".to_string()),
            "nbsp" => Some(" ".to_string()),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(entity[2..].trim(), 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
            }
            _ if entity.starts_with('#') => entity[1..]
                .trim()
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string()),
            _ => None,
        };
        match decoded {
            Some(text) => {
                out.push_str(&text);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    collapse_whitespace(&out)
}

fn collapse_whitespace(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
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

fn percent_decode(raw: &str) -> String {
    let mut bytes_out: Vec<u8> = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                bytes_out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            bytes_out.push(b' ');
        } else {
            bytes_out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&bytes_out).into_owned()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_key_vectors() {
        // www/case/trailing-slash/fragment collapse ...
        let cases = [
            ("https://example.com/a", "example.com/a"),
            ("https://WWW.EXAMPLE.COM/a", "example.com/a"),
            ("http://www.example.com/a#frag", "example.com/a"),
            ("https://example.com/a/", "example.com/a"),
        ];
        for (raw, want) in cases {
            assert_eq!(dedup_key(raw), want, "raw={raw}");
        }
        // ... but the query string is preserved (Omp `dedupKey`):
        // tracking-param variants do NOT deduplicate.
        assert_ne!(
            dedup_key("https://example.com/a?utm_source=x"),
            dedup_key("https://example.com/a"),
        );
    }

    fn hit(
        provider: SearchProvider,
        query: &str,
        rank: usize,
        url: &str,
        title: &str,
        snippet: &str,
    ) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            url: url.to_string(),
            snippet: snippet.to_string(),
            provider,
            query: query.to_string(),
            rank,
            published_date: None,
        }
    }

    #[test]
    fn merge_consensus_order() {
        // Two-provider group outranks any single-provider group, even with a
        // worse best rank; best-ranked title/URL adopted; longest snippet
        // wins; first-win date; URL-asc tie-break.
        let rows = vec![
            hit(
                SearchProvider::DuckDuckGo,
                "b",
                0,
                "https://solo.example/x",
                "Solo",
                "s",
            ),
            hit(
                SearchProvider::Startpage,
                "a",
                1,
                "https://example.com/shared",
                "SP title",
                "short",
            ),
            hit(
                SearchProvider::DuckDuckGo,
                "b",
                0,
                "https://example.com/shared?utm_source=x",
                "DDG title",
                "a much longer snippet",
            ),
            hit(
                SearchProvider::Startpage,
                "a",
                0,
                "https://example.com/shared?utm_source=x",
                "SP better",
                "mid",
            ),
        ];
        let merged = merge_sources(rows);
        // utm variant group has 2 providers -> first; bare-URL single next;
        // solo last (URL-asc tie-break among singletons by best rank).
        assert_eq!(merged.len(), 3);
        let top = &merged[0];
        assert_eq!(top.providers.len(), 2);
        assert_eq!(top.best_rank, 0);
        assert_eq!(top.title, "SP better");
        assert_eq!(top.snippet, "a much longer snippet");
        assert_eq!(top.canonical_url, "example.com/shared?utm_source=x");
        assert_eq!(top.display_url, "https://example.com/shared?utm_source=x");
        assert_eq!(top.hit_count, 2);
        assert_eq!(top.queries, vec!["a".to_string(), "b".to_string()],);
        // Singletons sort by provider count tie -> best_rank asc -> URL asc.
        assert_eq!(merged[1].best_rank, 0);
        assert_eq!(merged[1].canonical_url, "solo.example/x");
        assert_eq!(merged[2].canonical_url, "example.com/shared");
    }

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
    fn merge_input_order_beats_alphabetical_on_ties() {
        // Two Startpage legs whose query strings sort opposite to input
        // order: shared-URL adoption is strict `<` on rank, so equal ranks
        // keep the input-first leg's title. `merge_sources` falls back to
        // first-seen order for unknown queries; the order-aware core with the
        // input list proves the SPEC §8 contract directly.
        let rows = vec![
            hit(
                SearchProvider::Startpage,
                "zebra",
                0,
                "https://example.com/shared",
                "Zebra Title",
                "s",
            ),
            hit(
                SearchProvider::Startpage,
                "apple",
                0,
                "https://example.com/shared",
                "Apple Title",
                "s",
            ),
        ];
        let order = vec!["zebra".to_string(), "apple".to_string()];
        let merged = merge_sources_in_order(rows, &order);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "Zebra Title");
        assert_eq!(
            merged[0].queries,
            vec!["apple".to_string(), "zebra".to_string()]
        );
    }

    #[test]
    fn all_failed_message_format() {
        let msg = all_failed_message(&[
            ("duckduckgo".to_string(), "boom".to_string()),
            ("startpage".to_string(), "bust".to_string()),
        ]);
        assert_eq!(msg, "duckduckgo: boom; startpage: bust");
    }

    #[test]
    fn tool_schema_shape_and_roundtrip() {
        let schema = search_tool_schema();
        assert_eq!(schema["name"], json!("search"));
        // SPEC §5: `additionalProperties: false` everywhere — root, the
        // `parameters` object, and every `type: object` level below it. Only
        // `queries.items` is a non-object subschema and carries no flag.
        for pointer in ["", "/parameters"] {
            let node = schema.pointer(pointer).expect("schema node");
            assert_eq!(
                node.get("additionalProperties"),
                Some(&json!(false)),
                "missing flag at `{pointer}`"
            );
        }
        assert_eq!(schema["parameters"]["required"], json!(["queries"]));
        let queries = &schema["parameters"]["properties"]["queries"];
        assert_eq!(queries["minItems"], json!(1));
        assert_eq!(queries["maxItems"], json!(8));

        assert_eq!(queries["items"]["maxLength"], json!(500));
        let top_k = &schema["parameters"]["properties"]["top_k"];
        assert_eq!(top_k["minimum"], json!(1));
        assert_eq!(top_k["maximum"], json!(30));
        assert_eq!(top_k["default"], json!(15));
        let recency = &schema["parameters"]["properties"]["recency"];
        assert_eq!(recency["enum"], json!(["day", "week", "month", "year"]));
        // Valid payload validates ...
        assert!(SearchInput::validate(vec!["obscura browser".to_string()], Some(15), None).is_ok());
        assert!(
            SearchInput::validate(vec!["q".to_string()], Some(15), Some(Recency::Week),).is_ok()
        );
        // ... and invalid payloads fail: empty, too many, bad top_k clamps
        // (no error), over-long query, unknown recency string.
        assert_eq!(
            SearchInput::validate(vec![], None, None),
            Err(SearchProviderError::EmptyQuery)
        );
        assert!(SearchInput::validate(vec![" ".to_string()], None, None).is_err());
        let many: Vec<String> = (0..9).map(|i| format!("q{i}")).collect();
        assert_eq!(
            SearchInput::validate(many, None, None),
            Err(SearchProviderError::TooManyQueries { got: 9, max: 8 })
        );
        let long = "x".repeat(501);
        assert!(SearchInput::validate(vec![long], None, None).is_err());
        // 500 non-ASCII chars are 1000 bytes but legal: the bound counts
        // characters like the schema `maxLength`.
        assert!(SearchInput::validate(vec!["é".repeat(500)], None, None).is_ok());
        assert!(SearchInput::validate(vec!["é".repeat(501)], None, None).is_err());
        assert!(serde_json::from_str::<Recency>("\"fortnight\"").is_err());
        assert_eq!(Recency::Week.param(), "w");
        // Unknown properties are structurally rejected at every object level:
        // the schema declares no other keys beside the known ones.
        let known_root: Vec<&str> = schema
            .as_object()
            .expect("root object")
            .keys()
            .map(String::as_str)
            .collect();
        assert!(known_root.contains(&"$schema"));
        assert!(known_root.contains(&"name"));
        assert!(!known_root.contains(&"extra"));
        let known_params: Vec<&str> = schema["parameters"]
            .as_object()
            .expect("parameters object")
            .keys()
            .map(String::as_str)
            .collect();
        assert!(known_params.contains(&"properties"));
        assert!(!known_params.contains(&"extra"));
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
    fn timeout_beats_challenge() {
        let err = map_timeout(SearchProvider::DuckDuckGo, "q");
        assert_eq!(err.http_status(), 504);
        assert_eq!(err.code(), "timeout");
    }

    #[test]
    fn http_500_maps_503() {
        let err = map_ddg_response(500, "boom", "q").unwrap_err();
        assert_eq!(err.http_status(), 503);
        let err = map_startpage_response(500, "boom", "https://x/", "q").unwrap_err();
        assert_eq!(err.http_status(), 503);
    }

    #[test]
    fn validation_status_codes() {
        assert_eq!(SearchProviderError::EmptyQuery.http_status(), 400);
        assert_eq!(SearchProviderError::EmptyQuery.code(), "empty_query");
        assert_eq!(
            SearchProviderError::TooManyQueries { got: 9, max: 8 }.http_status(),
            400
        );
        assert_eq!(
            SearchProviderError::AllFailed {
                failures: "a: b".to_string()
            }
            .http_status(),
            503
        );
        assert_eq!(SearchProviderError::EmptyQuery.provider(), None);
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

    struct StubReply {
        status: u16,
        body: String,
    }

    impl StubReply {
        fn text(status: u16, body: &str) -> Self {
            StubReply {
                status,
                body: body.to_string(),
            }
        }
    }

    struct StubServer {
        base_url: String,
        hits: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
    }

    impl StubServer {
        fn base(&self) -> String {
            self.base_url.clone()
        }

        fn hits(&self, prefix: &str) -> usize {
            *self.hits.lock().expect("hits").get(prefix).unwrap_or(&0)
        }

        async fn serve(handler: fn(&str, &str) -> StubReply) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("stub bind");
            let addr = listener.local_addr().expect("stub addr");
            let hits: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
                Default::default();
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

        async fn serve_routes(cfg: FanoutStub) -> (String, StubHits) {
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

        async fn serve_slow(delay: std::time::Duration) -> Self {
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

    fn path_key(path: &str) -> String {
        // Count by route prefix so query strings do not split counters.
        for known in ["/html/", "/sp/search", "/sp/", "/"] {
            if path == known || path.starts_with(known) {
                return known.to_string();
            }
        }
        path.to_string()
    }

    fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        }
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

    type StubHits = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>;

    struct FanoutStub {
        ddg_page1: String,
        ddg_page2: String,
        sp_home: String,
        sp_search: String,
        sp_search_fail: bool,
    }

    impl FanoutStub {
        fn single_page(
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

    #[tokio::test]
    async fn fanout_merges_across_queries_and_providers() {
        // 3 queries x 2 providers over the local stub with overlapping URLs:
        // one group with 2 providers + 3 queries, stats.legs == 6.
        let shared = "https://example.com/shared";
        let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            ddg_rows(shared, "Shared DDG", "ddg text"),
            sp_home_form(),
            sp_rows(shared, "Shared SP", "sp text"),
            false,
        ))
        .await;
        let client = reqwest::Client::new();
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
        .expect("partial overlap still merges");
        assert_eq!(out.stats.legs, 6);
        assert_eq!(out.stats.queries, 3);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        let top = out
            .results
            .iter()
            .find(|r| r.canonical_url == "example.com/shared")
            .expect("shared group");
        assert_eq!(top.providers.len(), 2);
        assert_eq!(top.queries.len(), 3);
        assert_eq!(top.hit_count, 6);
        assert_eq!(out.stats.raw_hits, 6);
    }
    #[tokio::test]
    async fn fanout_duplicate_queries_keep_separate_legs() {
        // Duplicate query strings are legal input (`validate` does not
        // dedupe): both legs must report hits, not collapse into one slot
        // with a phantom `Timeout` on the other.
        let shared = "https://example.com/shared";
        let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            ddg_rows(shared, "Shared DDG", "ddg text"),
            sp_home_form(),
            sp_rows(shared, "Shared SP", "sp text"),
            false,
        ))
        .await;
        let client = reqwest::Client::new();
        let input = SearchInput::validate(
            vec!["same query".to_string(), "same query".to_string()],
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
        .expect("duplicate queries still merge");
        assert_eq!(out.stats.legs, 4);
        assert_eq!(out.stats.raw_hits, 4);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        let top = out
            .results
            .iter()
            .find(|r| r.canonical_url == "example.com/shared")
            .expect("shared group");
        assert_eq!(top.hit_count, 4);
    }

    #[tokio::test]
    async fn fanout_partial_failure_still_ok() {
        // One failing leg (Startpage POST 500) still yields Ok with 1 error
        // per Query (2 SP legs fail, 2 DDG legs succeed — one SP error per
        // query).
        let shared = "https://example.com/shared";
        let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            ddg_rows(shared, "Shared DDG", "ddg text"),
            sp_home_form(),
            "unused".to_string(),
            true,
        ))
        .await;
        let client = reqwest::Client::new();
        let input = SearchInput::validate(
            vec!["q one".to_string(), "q two".to_string()],
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
        .expect("partial success is Ok");
        assert_eq!(out.stats.legs, 4);
        assert_eq!(
            out.errors.len(),
            2,
            "one SP error per query: {:?}",
            out.errors
        );
        assert!(out.errors.iter().all(|e| e.http_status() == 503));
        assert!(!out.results.is_empty());
    }

    #[tokio::test]
    async fn fanout_all_fail_returns_all_failed_503() {
        // Every leg fails (DDG 404-route, Startpage POST 500) AND nothing
        // merges -> Err(AllFailed) with 503.
        let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            "unused".to_string(),
            sp_home_form(),
            "unused".to_string(),
            true,
        ))
        .await;
        let client = reqwest::Client::new();
        let input =
            SearchInput::validate(vec!["q one".to_string()], Some(15), None).expect("valid");
        // Point DDG at a missing route so it 404s -> Upstream.
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
