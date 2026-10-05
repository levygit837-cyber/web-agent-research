//! Parallel fan-out over the enabled provider legs with soft/hard
//! deadlines.
//!
//! One leg per agent-loop Query x enabled provider (#64: Startpage is off
//! by default). Hits are ordered by Reciprocal Rank Fusion (k=60) over
//! (engine, query) legs, with query coverage as the tiebreak; per-leg
//! errors collect into the output. Partial success is `Ok`; only
//! total-leg failure is `Err(AllFailed)`.
//!
//! Ported from oh-my-pi (MIT, can1357/oh-my-pi@83c9df0); see
//! `THIRD-PARTY-NOTICES.md`.

mod leg;
mod report;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::web::search::dedup::merge_sources_in_order;
use crate::web::search::dispatch::EngineBases;
use crate::web::search::governor::Governor;
use crate::web::search::types::{
    SearchInput, SearchOutput, SearchProvider, SearchProviderError, SearchResult, SearchStats,
    HARD_DEADLINE_SECS, SOFT_DEADLINE_SECS,
};
use leg::{LegEnv, LegOutcome};
pub use report::all_failed_message;
use report::{engine_status, error_detail};

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
/// everything else -> `Upstream` (503), `status: None`/`retry_after_secs:
/// None` (no response was ever received to read either from).
pub(crate) fn map_transport_error(
    provider: SearchProvider,
    query: &str,
    is_timeout: bool,
    detail: String,
) -> SearchProviderError {
    if is_timeout {
        map_timeout(provider, query)
    } else {
        SearchProviderError::Upstream {
            provider,
            detail,
            status: None,
            retry_after_secs: None,
        }
    }
}

/// Parse a response's `Retry-After` header, seconds form only (`Retry-After:
/// 120`); the HTTP-date form is rare from these providers and not worth the
/// parsing surface (#62: "Retry-After, when present, wins" -- a form we do
/// not parse is treated as absent, falling back to the default duration).
pub(crate) fn parse_retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Deep-module entry: full fan-out (validate is the caller's job -- this
/// takes a validated `SearchInput`; run legs under soft/hard deadlines ->
/// merge -> truncate). Accepts the shared client, base URLs (production
/// callers pass the real hosts; tests pass local `TcpListener` stubs --
/// `#[doc(hidden)]` since only the base-URL override makes this a test
/// seam, not the function's purpose), creates nothing. Leg order is
/// deterministic: per Query in input order, over
/// `governor.enabled_providers()` in priority order (#66: Startpage,
/// Brave, Yahoo, DuckDuckGo, Bing -- Startpage opt-in only). Never fails a
/// whole batch on one leg: leg errors collect into `output.errors`; only
/// total-leg failure (`AllFailed`: merged empty AND every leg failed)
/// returns `Err`. Otherwise `Ok` -- including partial success and empty
/// results (a recognized no-results page is not an error; a page with
/// neither rows nor a results/no-results marker is, see `markup`). Leg
/// panics surface as `Upstream { detail: "leg panicked" }`.
///
/// Deadlines (#104): every leg may run until the hard deadline
/// (`HARD_DEADLINE_SECS`). Once the soft deadline (`SOFT_DEADLINE_SECS`)
/// has passed and at least one leg succeeded, legs still queued for their
/// engine (not yet past `pace()`) are cancelled and report `Timeout`;
/// legs already past `pace()` -- whose request is on the wire -- are
/// awaited up to the hard deadline. `output.engine_status` carries one
/// line per enabled engine.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn search_multi_with_bases(
    client: &reqwest::Client,
    input: SearchInput,
    ddg_base: &str,
    sp_home: &str,
    sp_search: &str,
    brave_base: &str,
    yahoo_base: &str,
    bing_base: &str,
    governor: &Governor,
    cache_root: Option<&Path>,
) -> Result<SearchOutput, SearchProviderError> {
    let queries = input.queries.clone();
    let recency = input.recency;
    let top_k = input.top_k;
    // #64/#66: which engines run this call, in priority order (Startpage
    // first when enabled, then Brave, Yahoo, DuckDuckGo, Bing); the
    // hermetic `Governor` always enables all five, so every pre-existing
    // fan-out test that predates #66 keeps its original two-leg shape by
    // constructing `SEARCH_ENGINES`-filtered expectations explicitly where
    // it cares, and new tests cover the full five-leg shape directly.
    let mut enabled = governor.enabled_providers();
    let enabled_before_recency = enabled.len();
    // #81: with a `recency` window, a leg runs only if its engine applies
    // it. The others are not run unfiltered and are not legs at all: no
    // request, no `errors` entry, no suspension, no cache lookup or write.
    if let Some(window) = recency {
        enabled.retain(|provider| provider.applies_recency(window));
        if enabled.is_empty() {
            return Ok(SearchOutput {
                results: Vec::new(),
                errors: Vec::new(),
                stats: SearchStats {
                    queries: queries.len(),
                    legs: 0,
                    raw_hits: 0,
                    merged: 0,
                },
                note: Some(format!(
                    "recency `{}` is not supported by the enabled search engines; search again without recency.",
                    window.name()
                )),
                engine_status: Vec::new(),
            });
        }
    }
    // Engines skipped for `recency` were never asked, so a wall on the
    // remaining ones does not mean "every enabled engine is walled": the
    // same search without `recency` could still succeed.
    let narrowed_by_recency = enabled.len() < enabled_before_recency;
    let started_at = tokio::time::Instant::now();
    let soft_at = started_at + Duration::from_secs(SOFT_DEADLINE_SECS);
    let deadline = started_at + Duration::from_secs(HARD_DEADLINE_SECS);
    let leg_count = queries.len() * enabled.len();
    let env = Arc::new(LegEnv {
        client: client.clone(),
        bases: EngineBases {
            ddg: ddg_base.to_string(),
            sp_home: sp_home.to_string(),
            sp_search: sp_search.to_string(),
            brave: brave_base.to_string(),
            yahoo: yahoo_base.to_string(),
            bing: bing_base.to_string(),
        },
        governor: governor.clone(),
        cache_root: cache_root.map(Path::to_path_buf),
        recency,
        deadline,
        states: leg::queued_states(leg_count),
    });
    // Legs in deterministic order: per Query in input order, over `enabled`
    // in priority order. A leg's spawn index is its dense slot
    // (`query_index * enabled.len() + provider_index`): JoinSet returns
    // legs in completion order, so results reconstruct by index, never by
    // (query, provider) -- duplicate query strings keep separate slots, and
    // a panicked leg maps back to its slot via its task id.
    let mut set = tokio::task::JoinSet::new();
    let mut handles: Vec<tokio::task::AbortHandle> = Vec::with_capacity(leg_count);
    for (query_index, query) in queries.iter().enumerate() {
        for provider in enabled.iter().copied() {
            let index = handles.len();
            handles.push(set.spawn(leg::run(
                env.clone(),
                index,
                query_index,
                provider,
                query.clone(),
            )));
        }
    }
    // Cancel every leg still queued (not yet past `pace()`): it has sent
    // nothing, so aborting it costs no request.
    let cancel_queued = || {
        for (index, handle) in handles.iter().enumerate() {
            if env.cancel_if_queued(index) {
                handle.abort();
            }
        }
    };
    let mut by_leg: Vec<Option<LegOutcome>> = (0..leg_count).map(|_| None).collect();
    let mut successes: usize = 0;
    let mut soft_fired = false;
    loop {
        let next = if soft_fired {
            tokio::select! {
                next = set.join_next() => next,
                () = tokio::time::sleep_until(deadline) => break,
            }
        } else {
            tokio::select! {
                next = set.join_next() => next,
                () = tokio::time::sleep_until(soft_at) => {
                    soft_fired = true;
                    if successes >= 1 {
                        cancel_queued();
                    }
                    continue;
                }
            }
        };
        match next {
            Some(Ok((index, outcome))) => {
                if outcome.is_ok() {
                    successes += 1;
                    if soft_fired {
                        cancel_queued();
                    }
                }
                by_leg[index] = Some(outcome);
            }
            // Cancelled by the soft deadline: its slot reports `Timeout`.
            Some(Err(join)) if join.is_cancelled() => {}
            Some(Err(join)) => {
                // Leg task panicked: typed `Upstream` on its own slot, never
                // propagates.
                if let Some(index) = handles.iter().position(|h| h.id() == join.id()) {
                    by_leg[index] = Some(Err(SearchProviderError::Upstream {
                        provider: enabled[index % enabled.len()],
                        detail: "leg panicked".to_string(),
                        status: None,
                        retry_after_secs: None,
                    }));
                }
            }
            None => break,
        }
    }
    set.abort_all();
    // Legs cut by a deadline report `Timeout`, in leg order.
    let legs: Vec<(SearchProvider, LegOutcome)> = by_leg
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| {
            let provider = enabled[index % enabled.len()];
            let outcome = outcome
                .unwrap_or_else(|| Err(map_timeout(provider, &queries[index / enabled.len()])));
            (provider, outcome)
        })
        .collect();
    let status = engine_status(&enabled, &legs);
    let mut raw_hits: Vec<SearchResult> = Vec::new();
    let mut errors: Vec<SearchProviderError> = Vec::new();
    for (_, outcome) in legs {
        match outcome {
            Ok(rows) => raw_hits.extend(rows),
            Err(err) => errors.push(err),
        }
    }
    let raw_count = raw_hits.len();
    let mut merged = merge_sources_in_order(raw_hits, &queries);
    let merged_count = merged.len();
    if merged.is_empty() && errors.len() == leg_count && leg_count > 0 {
        // #62: a `Suspended` counts as a bot-wall signal only when its
        // `reason` is itself `"challenge"` (a suspension recorded from a
        // live Challenge) -- a `Suspended` from a 5xx/transport/429/403
        // failure, or a locally-scheduled `Throttled` skip, must never
        // force `all_challenged: true` (that would send a plain outage or
        // a busy-run cap down the exit-7 `SearchBlocked` path, which is
        // reserved for an actual bot wall).
        let all_challenged = !narrowed_by_recency
            && errors.iter().all(|err| match err {
                SearchProviderError::Challenge { .. } => true,
                SearchProviderError::Suspended { reason, .. } => reason == "challenge",
                _ => false,
            });
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
        note: None,
        engine_status: status,
    })
}

#[cfg(test)]
mod tests {
    mod cache;
    mod deadline;
    mod errors;
    mod governance;
    mod merge;
    mod recency;
}
