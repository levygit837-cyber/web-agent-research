//! One fan-out leg's lifecycle: cache lookup, per-call query cap,
//! suspension check, acquire and pace, engine request, outcome recording,
//! cache store.

use std::path::PathBuf;
use std::sync::Arc;

use crate::web::search::cache;
use crate::web::search::dispatch::{search_engine, EngineBases};
use crate::web::search::governor::{Governor, PaceOutcome};
use crate::web::search::types::{Recency, SearchProvider, SearchProviderError, SearchResult};

/// What one leg settled with.
pub(super) type LegOutcome = Result<Vec<SearchResult>, SearchProviderError>;

/// A leg's slot `(query_index, provider)`, the exact Query string, and its
/// outcome.
pub(super) type LegReport = ((usize, SearchProvider), String, LegOutcome);

/// Everything every leg of one fan-out call shares.
pub(super) struct LegEnv {
    pub(super) client: reqwest::Client,
    pub(super) bases: EngineBases,
    pub(super) governor: Governor,
    pub(super) cache_root: Option<PathBuf>,
    pub(super) recency: Option<Recency>,
}

/// Run one leg to completion.
pub(super) async fn run(
    env: Arc<LegEnv>,
    query_index: usize,
    provider: SearchProvider,
    query: String,
) -> LegReport {
    let slot = (query_index, provider);
    // Cache lookup per leg, not per merged output: a hit here for one
    // provider never blocks the other provider's live request for the same
    // Query. Checked before suspension/budget/pacing: a cache hit makes no
    // HTTP request, so none of those gates apply.
    if let Some(mut rows) =
        cache::lookup(env.cache_root.as_deref(), provider, &query, env.recency, 0)
    {
        // A cache hit may come from a differently-spelled Query that
        // normalizes to the same key (e.g. "Rust  async" vs "rust async");
        // re-stamp `query` with this call's exact string so
        // `merge_sources_in_order`'s `queries.position` lookup and the
        // documented `SearchResult.query` contract ("the exact Query
        // string that produced this hit") both hold on a hit, not only on
        // a live leg.
        for row in &mut rows {
            row.query = query.clone();
        }
        return (slot, query, Ok(rows));
    }
    let outcome = settle(&env, query_index, provider, &query).await;
    // Store only a settled `Ok` leg: a Challenge/Timeout/Upstream/
    // Suspended/Throttled `Err` never reaches `cache::store`.
    if let Ok(rows) = &outcome {
        cache::store(
            env.cache_root.as_deref(),
            provider,
            &query,
            env.recency,
            0,
            rows,
        );
    }
    (slot, query, outcome)
}

/// Gate, pace and run one live leg, recording its outcome with the
/// governor. The engine's permit is dropped on return, before the caller
/// stores anything in the cache.
async fn settle(
    env: &LegEnv,
    query_index: usize,
    provider: SearchProvider,
    query: &str,
) -> LegOutcome {
    let governor = &env.governor;
    // #63: cap on Queries dispatched to one engine within this call.
    // Checked after the cache lookup (a cache hit costs no request and so
    // never counts against the cap) and before suspension/budget:
    // capped-out queries are a local scheduling decision, not a bot-wall
    // signal, so they map to `Throttled`, never `Upstream`.
    if query_index >= governor.max_queries_per_engine(provider) {
        return Err(SearchProviderError::Throttled {
            provider,
            detail: format!(
                "{} per-call query cap reached ({} queries)",
                provider.id(),
                governor.max_queries_per_engine(provider)
            ),
        });
    }
    // #62: a suspended engine is skipped entirely -- no pacing wait, no
    // HTTP request -- before even acquiring a permit.
    if let Some((remaining_secs, reason)) = governor.suspended_remaining(provider) {
        return Err(SearchProviderError::Suspended {
            provider,
            remaining_secs,
            reason,
        });
    }
    // #63/#73: `acquire` blocks until this engine has a free
    // concurrency-limited permit (1 per engine pre-#73, up to
    // `resolve_concurrency` per engine since); `pace` then rechecks
    // suspension/budget (a sibling leg that acquired a permit ahead of this
    // one may have just settled and changed either) before waiting the
    // jittered gap from the last leg dispatched to this engine (persisted
    // across runs when a cache root is set) and reserving the per-run
    // budget.
    let permit = governor.acquire(provider).await;
    match permit.pace().await {
        PaceOutcome::Suspended {
            remaining_secs,
            reason,
        } => Err(SearchProviderError::Suspended {
            provider,
            remaining_secs,
            reason,
        }),
        PaceOutcome::Throttled => Err(SearchProviderError::Throttled {
            provider,
            detail: format!("{} request budget exhausted for this run", provider.id()),
        }),
        PaceOutcome::Proceed => {
            let result = search_engine(&env.client, provider, query, env.recency, &env.bases).await;
            match &result {
                Ok(_) => governor.record_success(provider),
                Err(err) => governor.record_failure(provider, err),
            }
            result
        }
    }
}
