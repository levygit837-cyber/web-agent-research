//! One fan-out leg's lifecycle: cache lookup, per-call query cap,
//! suspension check, acquire and pace, engine request (one transport
//! retry), outcome recording, cache store.
//!
//! Split out of the fan-out ported from oh-my-pi (MIT,
//! can1357/oh-my-pi@83c9df0); see `THIRD-PARTY-NOTICES.md`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::web::search::cache;
use crate::web::search::dispatch::{search_engine, EngineBases};
use crate::web::search::governor::{EnginePermit, Governor, PaceOutcome};
use crate::web::search::types::{Recency, SearchProvider, SearchProviderError, SearchResult};

/// What one leg settled with.
pub(super) type LegOutcome = Result<Vec<SearchResult>, SearchProviderError>;

/// A leg's spawn index (its dense slot,
/// `query_index * enabled.len() + provider_index`) and its outcome.
pub(super) type LegReport = (usize, LegOutcome);

/// Pause before the single retry of a transport failure (#103).
const TRANSPORT_RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// Leg has not yet passed its first `pace()`: still queued for its
/// engine's permit, or waiting out the pacing gap.
const QUEUED: u8 = 0;
/// Leg passed `pace()` and is about to send (or is sending) requests:
/// the soft deadline waits for it (#104).
const STARTED: u8 = 1;
/// The soft deadline cancelled the leg before it passed `pace()`.
const CANCELLED: u8 = 2;

/// Everything every leg of one fan-out call shares.
pub(super) struct LegEnv {
    pub(super) client: reqwest::Client,
    pub(super) bases: EngineBases,
    pub(super) governor: Governor,
    pub(super) cache_root: Option<PathBuf>,
    pub(super) recency: Option<Recency>,
    /// The fan-out's hard deadline: a transport retry that cannot finish
    /// before it is not attempted.
    pub(super) deadline: Instant,
    /// One [`QUEUED`]/[`STARTED`]/[`CANCELLED`] state per leg, by spawn
    /// index.
    pub(super) states: Vec<AtomicU8>,
}

impl LegEnv {
    /// Cancel leg `index` unless it already passed `pace()` (#104: the
    /// soft deadline aborts only legs still queued). `true` when it was
    /// cancelled; a started leg is left to finish.
    pub(super) fn cancel_if_queued(&self, index: usize) -> bool {
        self.states[index]
            .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Mark leg `index` started; `false` when the soft deadline already
    /// cancelled it.
    fn start(&self, index: usize) -> bool {
        self.states[index]
            .compare_exchange(QUEUED, STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Fresh per-leg states for `count` legs, every one still queued.
pub(super) fn queued_states(count: usize) -> Vec<AtomicU8> {
    (0..count).map(|_| AtomicU8::new(QUEUED)).collect()
}

/// Run one leg (spawn index `index`) to completion.
pub(super) async fn run(
    env: Arc<LegEnv>,
    index: usize,
    query_index: usize,
    provider: SearchProvider,
    query: String,
) -> LegReport {
    // Cache lookup per leg, not per merged output: a hit here for one
    // provider never blocks the other provider's live request for the same
    // Query. Checked before suspension/budget/pacing: a cache hit makes no
    // HTTP request, so none of those gates apply.
    if let Some(mut rows) =
        cache::lookup(env.cache_root.as_deref(), provider, &query, env.recency, 0)
    {
        // A cache hit may come from a differently-spelled Query that
        // normalizes to the same key (e.g. "Rust  async" vs "rust async");
        // re-stamp `query` with this call's exact string so the merge and
        // the documented `SearchResult.query` contract ("the exact Query
        // string that produced this hit") both hold on a hit, not only on
        // a live leg.
        for row in &mut rows {
            row.query = query.clone();
        }
        return (index, Ok(rows));
    }
    let outcome = settle(&env, index, query_index, provider, &query).await;
    // Store only a settled `Ok` leg with rows (#104): an `Err` never
    // reaches `cache::store`, and neither does a zero-row answer -- an
    // empty page is cheap to ask again and must not hide a later real
    // answer (or a drift that was briefly mistaken for one) for 24 h.
    if let Ok(rows) = &outcome {
        if !rows.is_empty() {
            cache::store(
                env.cache_root.as_deref(),
                provider,
                &query,
                env.recency,
                0,
                rows,
            );
        }
    }
    (index, outcome)
}

/// Gate, pace and run one live leg, recording its outcome with the
/// governor. The engine's permit is dropped on return, before the caller
/// stores anything in the cache.
async fn settle(
    env: &LegEnv,
    index: usize,
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
    // concurrency-limited permit; `pace` then refreshes shared state and
    // rechecks suspension/budget before waiting the jittered gap and
    // reserving the per-run budget.
    let permit = governor.acquire(provider).await;
    gate(&permit, provider).await?;
    // #104: past `pace()` the leg is "started": the soft deadline waits
    // for it instead of aborting it. A leg the soft deadline cancelled
    // while it was still queued (the abort can land just after `pace`
    // returned) sends nothing.
    if !env.start(index) {
        return Err(super::map_timeout(provider, query));
    }
    let mut result = search_engine(
        &env.client,
        provider,
        query,
        env.recency,
        &env.bases,
        &permit,
    )
    .await;
    // #103: a transport failure (no HTTP status: DNS, connect, TLS, body
    // read) never suspends; it is retried once, paced like any request,
    // when the retry still fits inside the fan-out's hard deadline.
    if is_transport_failure(&result) && Instant::now() + TRANSPORT_RETRY_BACKOFF < env.deadline {
        tokio::time::sleep(TRANSPORT_RETRY_BACKOFF).await;
        if gate(&permit, provider).await.is_ok() {
            result = search_engine(
                &env.client,
                provider,
                query,
                env.recency,
                &env.bases,
                &permit,
            )
            .await;
        }
    }
    match &result {
        Ok(_) => governor.record_success(provider),
        Err(err) => governor.record_failure(provider, err),
    }
    result
}

/// Pace one wire request on `permit`, mapping a refusal to its typed skip.
async fn gate(
    permit: &EnginePermit<'_>,
    provider: SearchProvider,
) -> Result<(), SearchProviderError> {
    match permit.pace().await {
        PaceOutcome::Proceed => Ok(()),
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
    }
}

fn is_transport_failure(result: &LegOutcome) -> bool {
    matches!(
        result,
        Err(SearchProviderError::Upstream { status: None, .. })
    )
}
