//! Parallel fan-out over both provider legs with soft/hard deadlines.
//!
//! One leg per agent-loop Query x provider, merged by consensus ranking;
//! per-leg errors collect into the output. Partial success is `Ok`; only
//! total-leg failure is `Err(AllFailed)`.

use std::path::Path;
use std::time::Duration;

use crate::web::search::cache;
use crate::web::search::ddg::{ddg_search, DDG_HTML_URL};
use crate::web::search::dedup::merge_sources_in_order;
use crate::web::search::startpage::{startpage_search, STARTPAGE_HOME_URL, STARTPAGE_SEARCH_URL};
use crate::web::search::types::{
    SearchInput, SearchOutput, SearchProvider, SearchProviderError, SearchResult, SearchStats,
    HARD_DEADLINE_SECS, SOFT_DEADLINE_SECS,
};

/// Build the typed leg-timeout error. Timeout beats Challenge: callers map
/// transport timeouts (or fan-out deadline cuts) here before inspecting any
/// body, so a slow challenge page still reports `Timeout` (504).
pub(crate) fn map_timeout(provider: SearchProvider, query: &str) -> SearchProviderError {
    SearchProviderError::Timeout {
        provider,
        query: query.to_string(),
    }
}

/// Map a `reqwest` transport error for one leg: timeouts -> `Timeout` (504),
/// everything else -> `Upstream` (503). Pure over the error predicates so
/// unit tests pin the mapping without network.
pub(crate) fn map_transport_error(
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
    cache_root: Option<&Path>,
) -> Result<SearchOutput, SearchProviderError> {
    search_multi_with_bases(
        client,
        input,
        DDG_HTML_URL,
        STARTPAGE_HOME_URL,
        STARTPAGE_SEARCH_URL,
        cache_root,
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
    cache_root: Option<&Path>,
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
            let cache_root_owned = cache_root.map(Path::to_path_buf);
            let slot = (query_index, provider);
            leg_slots.push(slot);
            leg_ids.push(
                set.spawn(async move {
                    // Cache lookup per leg, not per merged output: a hit
                    // here for one provider never blocks the other
                    // provider's live request for the same Query.
                    if let Some(mut rows) = cache::lookup(
                        cache_root_owned.as_deref(),
                        provider,
                        &query_owned,
                        recency,
                        0,
                    ) {
                        // A cache hit may come from a differently-spelled
                        // Query that normalizes to the same key (e.g.
                        // "Rust  async" vs "rust async"); re-stamp `query`
                        // with this call's exact string so
                        // `merge_sources_in_order`'s `queries.position`
                        // lookup and the documented `SearchResult.query`
                        // contract ("the exact Query string that produced
                        // this hit") both hold on a hit, not only on a
                        // live leg.
                        for row in &mut rows {
                            row.query = query_owned.clone();
                        }
                        return (slot, query_owned, Ok(rows));
                    }
                    let outcome: Result<Vec<SearchResult>, SearchProviderError> = match provider {
                        SearchProvider::Startpage => {
                            startpage_search(&client_owned, &query_owned, recency, &home, &search)
                                .await
                        }
                        SearchProvider::DuckDuckGo => {
                            ddg_search(&client_owned, &query_owned, recency, &ddg).await
                        }
                    };
                    // Store only a settled `Ok` leg: a Challenge/Timeout/
                    // Upstream `Err` never reaches `cache::store`.
                    if let Ok(rows) = &outcome {
                        cache::store(
                            cache_root_owned.as_deref(),
                            provider,
                            &query_owned,
                            recency,
                            0,
                            rows,
                        );
                    }
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
        let all_challenged = errors
            .iter()
            .all(|err| matches!(err, SearchProviderError::Challenge { .. }));
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
            all_challenged,
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
        SearchProviderError::AllFailed { failures, .. } => failures.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::test_support::EnvGuard;
    use super::super::ddg::map_ddg_response;
    use super::super::startpage::map_startpage_response;
    use super::super::test_support::{ddg_rows, sp_home_form, sp_rows, FanoutStub, StubServer};
    use super::*;

    #[test]
    fn all_failed_message_format() {
        let msg = all_failed_message(&[
            ("duckduckgo".to_string(), "boom".to_string()),
            ("startpage".to_string(), "bust".to_string()),
        ]);
        assert_eq!(msg, "duckduckgo: boom; startpage: bust");
    }

    #[tokio::test]
    async fn repeat_search_with_cache_root_makes_zero_http_requests() {
        // Acceptance (#67): a second identical `search_multi_with_bases`
        // call against the same `cache_root` must not touch the network at
        // all -- proven here by the local stub's per-route hit counter
        // staying flat across the second call, not just by asserting the
        // returned rows match. Holds the same env lock as `cache::tests`:
        // this test depends on `SEARCH_CACHE`/`SEARCH_CACHE_TTL_SECS`/
        // `SEARCH_CACHE_MAX_BYTES` staying at their defaults, which those
        // tests mutate in the same process.
        let _guard = EnvGuard::lock();
        let root = std::env::temp_dir().join(format!(
            "war-fanout-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("create temp cache root");
        let shared = "https://example.com/cached";
        let (base, hits) = StubServer::serve_routes(FanoutStub::single_page(
            ddg_rows(shared, "Cached DDG", "ddg text"),
            sp_home_form(),
            sp_rows(shared, "Cached SP", "sp text"),
            false,
        ))
        .await;
        let client = reqwest::Client::new();
        let make_input = || {
            SearchInput::validate(vec!["cached query".to_string()], Some(15), None).expect("valid")
        };
        let first = search_multi_with_bases(
            &client,
            make_input(),
            &format!("{base}/html/"),
            &format!("{base}/"),
            &format!("{base}/sp/search"),
            Some(&root),
        )
        .await
        .expect("first call succeeds and populates the cache");
        assert!(
            !first.results.is_empty(),
            "first call must actually hit the stub"
        );
        let html_hits_after_first = hits
            .lock()
            .expect("hits")
            .get("/html/")
            .copied()
            .unwrap_or(0);
        let sp_hits_after_first = hits
            .lock()
            .expect("hits")
            .get("/sp/search")
            .copied()
            .unwrap_or(0);
        assert!(
            html_hits_after_first > 0,
            "DDG leg must have hit the stub once"
        );
        assert!(
            sp_hits_after_first > 0,
            "Startpage leg must have hit the stub once"
        );
        let second = search_multi_with_bases(
            &client,
            make_input(),
            &format!("{base}/html/"),
            &format!("{base}/"),
            &format!("{base}/sp/search"),
            Some(&root),
        )
        .await
        .expect("second call is served entirely from cache");
        assert_eq!(
            hits.lock()
                .expect("hits")
                .get("/html/")
                .copied()
                .unwrap_or(0),
            html_hits_after_first,
            "a second identical search must make zero DDG HTTP requests"
        );
        assert_eq!(
            hits.lock()
                .expect("hits")
                .get("/sp/search")
                .copied()
                .unwrap_or(0),
            sp_hits_after_first,
            "a second identical search must make zero Startpage HTTP requests"
        );
        assert_eq!(
            second.results, first.results,
            "cached rows must round-trip identically"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn challenged_leg_is_never_cached() {
        // Acceptance (#67): "a Challenge is never cached." Drives the real
        // fan-out call site (not a reimplemented `if let Ok` in the test)
        // against a stub that Challenge-walls both legs: a second call
        // must hit the stub again, and the cache directory must end up
        // with no entries, because `store` is inside `if let Ok(rows) =
        // &outcome` at the leg closure and a Challenge is an `Err`.
        let _guard = EnvGuard::lock();
        let root = std::env::temp_dir().join(format!(
            "war-fanout-challenge-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("create temp cache root");
        let (base, hits) = StubServer::serve_routes(FanoutStub::single_page(
            r#"<div id="anomaly-modal"></div>"#.to_string(),
            sp_home_form(),
            r#"<script id="anubis_challenge" type="application/json">{}</script>"#.to_string(),
            false,
        ))
        .await;
        let client = reqwest::Client::new();
        let make_input = || {
            SearchInput::validate(vec!["walled query".to_string()], Some(15), None).expect("valid")
        };
        let first_err = search_multi_with_bases(
            &client,
            make_input(),
            &format!("{base}/html/"),
            &format!("{base}/"),
            &format!("{base}/sp/search"),
            Some(&root),
        )
        .await
        .expect_err("both legs Challenge -> AllFailed, nothing merges");
        assert_eq!(first_err.code(), "all_failed");
        let html_hits_after_first = hits
            .lock()
            .expect("hits")
            .get("/html/")
            .copied()
            .unwrap_or(0);
        assert!(
            html_hits_after_first > 0,
            "the DDG leg must have hit the stub"
        );
        // The cache must hold no entry for the Challenged leg.
        let search_dir = root.join("search");
        let entry_count = std::fs::read_dir(&search_dir)
            .map(|read_dir| read_dir.flatten().count())
            .unwrap_or(0);
        assert_eq!(
            entry_count, 0,
            "a Challenge leg must never write a cache entry"
        );
        // A second call must hit the stub again -- proof by absence of a
        // cache short-circuit, not merely by inspecting the directory.
        let second_err = search_multi_with_bases(
            &client,
            make_input(),
            &format!("{base}/html/"),
            &format!("{base}/"),
            &format!("{base}/sp/search"),
            Some(&root),
        )
        .await
        .expect_err("still Challenge-walled, no cache to short-circuit it");
        assert_eq!(second_err.code(), "all_failed");
        assert!(
            hits.lock().expect("hits").get("/html/").copied().unwrap_or(0) > html_hits_after_first,
            "a Challenge leg must never be served from cache: the second call must hit the stub again"
        );
        std::fs::remove_dir_all(&root).ok();
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
                failures: "a: b".to_string(),
                all_challenged: false,
            }
            .http_status(),
            503
        );
        assert_eq!(SearchProviderError::EmptyQuery.provider(), None);
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
            None,
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
            None,
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
            None,
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
            None,
        )
        .await
        .expect_err("all legs fail -> AllFailed");
        assert_eq!(err.http_status(), 503);
        assert_eq!(err.code(), "all_failed");
        match &err {
            SearchProviderError::AllFailed { all_challenged, .. } => {
                assert!(
                    !all_challenged,
                    "a 404 Upstream leg mixed with a 500 Upstream leg is not all-Challenge"
                );
            }
            other => panic!("expected AllFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fanout_all_challenged_marks_all_failed() {
        // Every leg reports a bot-wall Challenge (DDG anomaly body, Startpage
        // Anubis PoW body via the search POST) AND nothing merges -> Err
        // (AllFailed) with `all_challenged: true` (#53): the signal
        // `research::agent_loop` uses to surface `SearchBlocked` to the
        // Harness instead of a silent empty Synthesis.
        let (base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            r#"<div id="anomaly-modal"></div>"#.to_string(),
            sp_home_form(),
            r#"<script id="anubis_challenge" type="application/json">{}</script>"#.to_string(),
            false,
        ))
        .await;
        let client = reqwest::Client::new();
        let input =
            SearchInput::validate(vec!["q one".to_string()], Some(15), None).expect("valid");
        let err = search_multi_with_bases(
            &client,
            input,
            &format!("{base}/html/"),
            &format!("{base}/"),
            &format!("{base}/sp/search"),
            None,
        )
        .await
        .expect_err("both legs Challenge -> AllFailed");
        assert_eq!(err.http_status(), 503);
        assert_eq!(err.code(), "all_failed");
        match &err {
            SearchProviderError::AllFailed { all_challenged, .. } => {
                assert!(all_challenged, "both legs were Challenge, must be true");
            }
            other => panic!("expected AllFailed, got {other:?}"),
        }
    }
}
