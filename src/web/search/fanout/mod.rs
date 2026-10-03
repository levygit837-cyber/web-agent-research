//! Parallel fan-out over both provider legs with soft/hard deadlines.
//!
//! One leg per agent-loop Query x enabled provider (#64: Startpage is off
//! by default), merged by consensus ranking; per-leg errors collect into
//! the output. Partial success is `Ok`; only total-leg failure is
//! `Err(AllFailed)`.
//!
//! Ported from oh-my-pi (MIT, can1357/oh-my-pi@83c9df0); see
//! `THIRD-PARTY-NOTICES.md`.

mod leg;

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

/// Format leg failures as `"id: msg; id: msg"` (Omp
/// `formatSearchProviderFailures`).
pub fn all_failed_message(failures: &[(String, String)]) -> String {
    failures
        .iter()
        .map(|(id, msg)| format!("{id}: {msg}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Leg index for `(query_index, provider)` given the enabled-provider list
/// (#64: Startpage may be absent), so the settle-reconstruction map stays a
/// dense `0..queries.len() * enabled.len()` range regardless of which, or
/// how many, providers ran.
fn leg_slot(query_index: usize, provider: SearchProvider, enabled: &[SearchProvider]) -> usize {
    let provider_index = enabled
        .iter()
        .position(|p| *p == provider)
        .expect("leg_slot only called for an enabled provider");
    query_index * enabled.len() + provider_index
}

/// One settled fan-out leg: spawn index, provider, query, and outcome.
type SettledLeg = (usize, SearchProvider, String, LegOutcome);
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
/// results (zero results with no challenge marker is not an error). Leg
/// panics surface as `Upstream { detail: "leg panicked" }`.
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
            });
        }
    }
    // Engines skipped for `recency` were never asked, so a wall on the
    // remaining ones does not mean "every enabled engine is walled": the
    // same search without `recency` could still succeed.
    let narrowed_by_recency = enabled.len() < enabled_before_recency;
    // Legs in deterministic order: per Query in input order, over `enabled`
    // in priority order. Each leg keeps its spawn index (query index +
    // provider): JoinSet returns legs in completion order, so results
    // reconstruct by index, never by (query, provider) — duplicate query
    // strings no longer collapse, and a panicked leg maps back to its slot
    // via JoinError::id.
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
    });
    let mut set = tokio::task::JoinSet::new();
    let leg_count = queries.len() * enabled.len();
    let mut leg_ids: Vec<tokio::task::Id> = Vec::with_capacity(leg_count);
    let mut leg_slots: Vec<(usize, SearchProvider)> = Vec::with_capacity(leg_count);
    for (query_index, query) in queries.iter().enumerate() {
        for provider in enabled.iter().copied() {
            leg_slots.push((query_index, provider));
            leg_ids.push(
                set.spawn(leg::run(env.clone(), query_index, provider, query.clone()))
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
                    .unwrap_or((0, enabled[0]));
                let query = queries.get(query_index).cloned().unwrap_or_default();
                settled.push((
                    query_index,
                    provider,
                    query,
                    Err(SearchProviderError::Upstream {
                        provider,
                        detail: "leg panicked".to_string(),
                        status: None,
                        retry_after_secs: None,
                    }),
                ));
            }
            None => break,
        }
    }
    set.abort_all();
    // Legs cut by the deadlines report `Timeout`. Reconstruct leg order by
    // spawn index: settled legs keep results; missing slots (per Query in
    // input order, over `enabled` in priority order) time out. Duplicate
    // query strings keep separate slots.
    let mut by_leg: std::collections::HashMap<usize, LegOutcome> = std::collections::HashMap::new();
    for (query_index, provider, _query, outcome) in settled {
        by_leg.insert(leg_slot(query_index, provider, &enabled), outcome);
    }
    let mut raw_hits: Vec<SearchResult> = Vec::new();
    let mut errors: Vec<SearchProviderError> = Vec::new();
    for (query_index, query) in queries.iter().enumerate() {
        for provider in enabled.iter().copied() {
            let slot = leg_slot(query_index, provider, &enabled);
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
        SearchProviderError::Suspended {
            remaining_secs,
            reason,
            ..
        } => format!("suspended ({reason}), {remaining_secs}s left"),
        SearchProviderError::Throttled { detail, .. } => detail.clone(),
        SearchProviderError::AllFailed { failures, .. } => failures.clone(),
    }
}

#[cfg(test)]
mod tests {
    mod cache;
    mod errors;
    mod governance;
    mod merge;
    mod recency;
}
