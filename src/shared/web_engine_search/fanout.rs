//! Parallel fan-out over both provider legs with soft/hard deadlines.
//!
//! One leg per agent-loop Query x provider, merged by consensus ranking;
//! per-leg errors collect into the output. Partial success is `Ok`; only
//! total-leg failure is `Err(AllFailed)`.

use std::time::Duration;

use crate::shared::types::search::{
    SearchInput, SearchOutput, SearchProvider, SearchProviderError, SearchResult, SearchStats,
    HARD_DEADLINE_SECS, SOFT_DEADLINE_SECS,
};
use crate::shared::web_engine_search::ddg::{ddg_search, DDG_HTML_URL};
use crate::shared::web_engine_search::dedup::merge_sources_in_order;
use crate::shared::web_engine_search::startpage::{
    startpage_search, STARTPAGE_HOME_URL, STARTPAGE_SEARCH_URL,
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

#[cfg(test)]
mod tests {
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
}
