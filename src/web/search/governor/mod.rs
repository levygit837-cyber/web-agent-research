//! Per-engine suspension (#62) and human pacing (#63) for the fetch-only
//! search fan-out.
//!
//! Idea only, no code: SearXNG's `search.suspended_times` (AGPL-3.0,
//! `searx/settings.yml`) inspired the error-kind -> duration mapping; every
//! line below is an independent implementation.
//!
//! Suspension and pacing both compare against `tokio::time::Instant`, never
//! wall clock directly, so `tokio::time::pause`/`advance` drive both
//! deterministically in hermetic tests. Wall clock (`SystemTime`) is used
//! only to bridge a persisted timestamp across process boundaries: once at
//! load (`wall_to_mono`) and once at save (`mono_to_wall`), both relative to
//! one `(base_wall_ms, base_mono)` pair fixed at construction. A persisted
//! timestamp is loaded lazily (`ensure_loaded`, on first real use of a
//! non-hermetic `Governor`), not eagerly at construction: `Searcher::new()`
//! is not fallible and must not touch disk before the first `search` call.
//!
//! One engine's requests never overlap: [`Governor::acquire`] returns an
//! [`EnginePermit`] that holds this engine's serial queue for the whole
//! leg (however many wire requests it turns out to need -- DDG's page-1
//! POST plus any continuation re-POSTs, Startpage's homepage GET plus its
//! search POST/GET fallback); [`EnginePermit::pace`], called once right
//! after acquiring and before the leg's first wire request, rechecks
//! suspension and the per-run budget (closing the race where a sibling
//! leg for the same engine settles -- and suspends the engine, or
//! exhausts the budget -- while this leg was still queued) and then waits
//! the jittered gap since the last *leg* dispatched to this engine. A
//! multi-request leg's own internal follow-up requests (DDG continuation
//! pages, Startpage's GET fallback) are not independently gapped or
//! re-checked; only each leg's first request is.
//!
//! Hermetic rule: [`Governor::hermetic`] (used by `Searcher::with_bases`
//! and every existing fan-out test) never persists, never paces, and always
//! enables both providers regardless of `SEARCH_ENGINES` -- the engine
//! allowlist (#64) is a production-path concern, tested directly via
//! [`enabled_providers`] and via [`Governor::new`] with an opt-in temp dir,
//! never through the hermetic seam. [`Governor::new`] is the production
//! (and opt-in-test) path: only the two request caps
//! (`SEARCH_MAX_REQUESTS_PER_ENGINE`/`SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL`)
//! are read once, at construction; the engine allowlist, suspension
//! durations, and pacing gaps are re-read from env on every call
//! ([`enabled_providers`], [`suspension_for`], [`resolve_gap`]), so a test
//! that mutates `SEARCH_*` between two calls on the same `Governor` sees
//! the new value immediately. State persists to
//! `<cache_root>/engines.json` when a cache root is given, in-memory-only
//! otherwise.
//!
//! #73 adds per-engine concurrency: each engine's serial queue becomes a
//! `Semaphore` sized by [`resolve_concurrency`] (1 permit = the original
//! "never overlap" behavior; Bing/Yahoo default to 2, per live
//! measurement). A hermetic `Governor` always uses 1 permit per engine
//! regardless of the production default, since the hermetic seam tests
//! merge/fan-out logic, not pacing/concurrency.

mod config;
mod permit;
mod persist;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tokio::time::Instant;

use crate::web::cache_dir::write_atomic;
use crate::web::search::types::{SearchProvider, SearchProviderError};
pub(crate) use config::resolve_max_pages;
use config::{
    enabled_providers, resolve_caps_per_engine, resolve_concurrency,
    resolve_max_queries_per_engine, suspension_for,
};
pub(crate) use permit::{EnginePermit, PaceOutcome};
use persist::{
    load_persisted, mono_to_wall, now_wall_ms, wall_to_mono, PersistedEngine, PersistedState,
    ENGINES_FILE,
};

/// Every ported provider, in a fixed order used to pre-populate per-engine
/// maps regardless of which are enabled.
const ALL_PROVIDERS: [SearchProvider; 5] = [
    SearchProvider::Startpage,
    SearchProvider::Brave,
    SearchProvider::Yahoo,
    SearchProvider::DuckDuckGo,
    SearchProvider::Bing,
];

/// In-memory, monotonic-clock view of one engine's governed state.
#[derive(Debug, Default, Clone)]
struct EngineView {
    suspended_until: Option<Instant>,
    suspend_reason: String,
    last_request: Option<Instant>,
    request_count: u32,
}

#[derive(Debug)]
struct GovernorInner {
    cache_root: Option<PathBuf>,
    /// `true` for [`Governor::hermetic`]: suspension/pacing/budget checks
    /// are all no-ops and `enabled_providers` always returns both engines,
    /// regardless of ambient `SEARCH_*` env vars.
    hermetic: bool,
    /// `true` once `engines.json` has been read into `views` (or would be,
    /// for a hermetic `Governor`, which starts `true` and never loads).
    /// Guards a one-time lazy load on first real use, so `Governor::new`
    /// (and therefore `Searcher::new`) never touches disk before the first
    /// suspension check or paced call.
    loaded: AtomicBool,
    views: Mutex<HashMap<SearchProvider, EngineView>>,
    /// One concurrency-limited queue per engine (#73: a `Semaphore` sized
    /// by [`resolve_concurrency`] per engine -- 1 permit degenerates to
    /// the pre-#73 "serial per engine" behavior; 2+ permits let that many
    /// requests to the *same* engine be genuinely in flight at once,
    /// matching what #73's live measurement found each engine tolerates.
    /// An [`EnginePermit`] holds one permit for its entire leg, however
    /// many wire requests that leg turns out to need; different engines
    /// have independent semaphores and therefore always run in parallel
    /// with each other (#63) regardless of any one engine's limit. A
    /// hermetic `Governor` sizes every engine's semaphore to exactly 1
    /// permit (the pre-#73 behavior), since the hermetic seam tests
    /// merge/fan-out logic, not pacing/concurrency (see the module doc).
    queues: HashMap<SearchProvider, tokio::sync::Semaphore>,
    /// Serializes `persist`'s snapshot-then-write so two engines'
    /// concurrent persists cannot race: without this, engine A could
    /// snapshot, engine B could snapshot and rename first, then A's
    /// (older) snapshot renames last and silently drops B's update.
    persist_lock: Mutex<()>,
    /// Per-engine per-run request cap (#73: [`resolve_caps_per_engine`]);
    /// `u32::MAX` for every provider on a hermetic `Governor` (no cap).
    max_requests_per_engine: HashMap<SearchProvider, u32>,
    /// Per-engine cap on Queries dispatched to that engine within a
    /// single `search_multi_with_bases` call (#63, #73:
    /// [`resolve_max_queries_per_engine`]); `usize::MAX` for every
    /// provider on a hermetic `Governor` (no cap). See
    /// [`Governor::max_queries_per_engine`] for the deadline math behind
    /// the default.
    max_queries_per_engine: HashMap<SearchProvider, usize>,
    base_wall_ms: i64,
    base_mono: Instant,
}

/// Build every provider's concurrency semaphore. `hermetic = true` gives
/// every engine exactly 1 permit (the pre-#73 "serial per engine"
/// behavior the hermetic seam has always had); `hermetic = false` sizes
/// each engine from [`resolve_concurrency`] (#73).
fn build_queues(hermetic: bool) -> HashMap<SearchProvider, tokio::sync::Semaphore> {
    ALL_PROVIDERS
        .into_iter()
        .map(|p| {
            let permits = if hermetic { 1 } else { resolve_concurrency(p) };
            (p, tokio::sync::Semaphore::new(permits))
        })
        .collect()
}

/// Every provider mapped to the same `value` -- used by
/// [`Governor::hermetic`] to give every engine an unbounded cap.
fn all_providers_with<T: Copy>(value: T) -> HashMap<SearchProvider, T> {
    ALL_PROVIDERS.into_iter().map(|p| (p, value)).collect()
}

/// Cheap-to-clone handle (`Arc` inside) shared across every spawned leg of
/// one fan-out call. `#[doc(hidden)] pub` only where an external test seam
/// needs it (`Searcher::with_bases`'s hermetic default and
/// `search_multi_with_bases`'s signature); every other member stays
/// `pub(crate)`.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct Governor(std::sync::Arc<GovernorInner>);

impl Governor {
    /// Hermetic seam: no persisted state, zero pacing delay, no per-run or
    /// per-call cap, both providers always enabled. Used by
    /// `Searcher::with_bases` and every pre-existing fan-out test; the
    /// #62/#63/#64 features themselves are tested via `enabled_providers`
    /// directly and via [`Governor::new`] with an explicit temp dir.
    #[doc(hidden)]
    pub fn hermetic() -> Self {
        Governor(std::sync::Arc::new(GovernorInner {
            cache_root: None,
            hermetic: true,
            loaded: AtomicBool::new(true),
            views: Mutex::new(HashMap::new()),
            queues: build_queues(true),
            persist_lock: Mutex::new(()),
            max_requests_per_engine: all_providers_with(u32::MAX),
            max_queries_per_engine: all_providers_with(usize::MAX),
            base_wall_ms: now_wall_ms(),
            base_mono: Instant::now(),
        }))
    }

    /// Production seam: engine allowlist, suspension durations, pacing
    /// gaps, and both caps all come from `SEARCH_*` env vars read once
    /// here. `<cache_root>/engines.json`, when `cache_root` is `Some`, is
    /// read lazily on first real use ([`ensure_loaded`](Self::acquire)),
    /// not here: construction never touches disk. A corrupt or missing
    /// `engines.json` loads as empty state, never an error.
    pub(crate) fn new(cache_root: Option<PathBuf>) -> Self {
        let max_requests_per_engine = resolve_caps_per_engine();
        // #63: cap on Queries dispatched to one engine within a single
        // `search_multi_with_bases` call, accounting for the pacing gap so
        // the soft/hard fan-out deadlines (5 s / 30 s,
        // `SOFT_DEADLINE_SECS`/`HARD_DEADLINE_SECS`) still make sense once
        // requests to one engine are serialized with a jittered gap
        // between them instead of firing in parallel. Default 4: with the
        // default pacing gap (`SEARCH_PACE_MAX_MS`, 4000 ms worst case),
        // the 4th query to one engine waits at most 3 * 4000 ms = 12 s of
        // pure pacing before its request even starts, leaving 18 of the
        // 30 s hard deadline for the round-trips themselves (measured live
        // fan-out latency is ~1.7 s per `docs/harness.md`, so 4 serial
        // round-trips comfortably fit). `MAX_QUERIES` (8) queries all
        // landing on one engine (e.g. `SEARCH_ENGINES=duckduckgo` and 8
        // input Queries) would otherwise let the 8th wait up to 7 * 4000 ms
        // = 28 s before starting -- past the soft deadline and eating
        // nearly the whole hard one. Bing/Yahoo's tighter #73 gap default
        // (1000..2000 ms) leaves even more headroom for those two.
        let max_queries_per_engine = resolve_max_queries_per_engine();
        Governor(std::sync::Arc::new(GovernorInner {
            cache_root,
            hermetic: false,
            loaded: AtomicBool::new(false),
            views: Mutex::new(HashMap::new()),
            queues: build_queues(false),
            persist_lock: Mutex::new(()),
            max_requests_per_engine,
            max_queries_per_engine,
            base_wall_ms: now_wall_ms(),
            base_mono: Instant::now(),
        }))
    }

    /// One-time lazy load of `<cache_root>/engines.json` into `views`, on
    /// first real use of a non-hermetic `Governor` with a cache root.
    /// Idempotent (an `AtomicBool` swap guards the actual read): safe to
    /// call at the top of every accessor that needs persisted state.
    fn ensure_loaded(&self) {
        if self.0.loaded.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(cache_root) = &self.0.cache_root else {
            return;
        };
        let persisted = load_persisted(cache_root);
        let mut views = self.0.views.lock().expect("governor views lock");
        for provider in ALL_PROVIDERS {
            let entry = persisted.engines.get(provider.id());
            let view = views.entry(provider).or_default();
            view.suspended_until = entry
                .and_then(|e| e.suspended_until_ms)
                .and_then(|ms| wall_to_mono(ms, self.0.base_wall_ms, self.0.base_mono));
            view.suspend_reason = entry
                .and_then(|e| e.suspend_reason.clone())
                .unwrap_or_default();
            view.last_request = entry
                .and_then(|e| e.last_request_ms)
                .and_then(|ms| wall_to_mono(ms, self.0.base_wall_ms, self.0.base_mono));
        }
    }

    /// Providers this fan-out call should attempt, in merge-priority order
    /// (#64: Startpage is off by default; see [`enabled_providers`]).
    pub(crate) fn enabled_providers(&self) -> Vec<SearchProvider> {
        if self.0.hermetic {
            ALL_PROVIDERS.to_vec()
        } else {
            enabled_providers()
        }
    }

    /// Cap on Queries dispatched to `provider` within a single fan-out
    /// call (#63, `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL[_<ENGINE>]`,
    /// default 4); `usize::MAX` for a hermetic `Governor` (no cap).
    pub(crate) fn max_queries_per_engine(&self, provider: SearchProvider) -> usize {
        self.0
            .max_queries_per_engine
            .get(&provider)
            .copied()
            .unwrap_or(usize::MAX)
    }

    /// `Some((remaining_secs, reason))` if `provider` is currently under an
    /// active suspension; `None` otherwise (including every hermetic
    /// call). Compares against `Instant::now()`, so a paused tokio clock
    /// drives expiry deterministically in tests.
    pub(crate) fn suspended_remaining(&self, provider: SearchProvider) -> Option<(u64, String)> {
        if !self.0.hermetic {
            self.ensure_loaded();
        }
        let views = self.0.views.lock().expect("governor views lock");
        let view = views.get(&provider)?;
        let until = view.suspended_until?;
        let now = Instant::now();
        if until <= now {
            return None;
        }
        let remaining = until.saturating_duration_since(now).as_secs().max(1);
        Some((remaining, view.suspend_reason.clone()))
    }

    /// `true` once `provider` has reached its per-run request cap (#73:
    /// [`resolve_caps_per_engine`],
    /// `SEARCH_MAX_REQUESTS_PER_ENGINE[_<ENGINE>]`); always `false` for a
    /// hermetic `Governor`. A cheap up-front check so a leg that is
    /// already over budget skips queueing on this engine entirely; the
    /// actual enforcement (closing the race a plain check-then-later-
    /// increment would leave open under concurrent legs) is
    /// [`EnginePermit::pace`]'s check-and-reserve, which runs while this
    /// engine's queue permit is held and so can never race with a sibling
    /// leg for the same engine beyond this engine's own concurrency
    /// limit.
    pub(crate) fn over_budget(&self, provider: SearchProvider) -> bool {
        if self.0.hermetic {
            return false;
        }
        let cap = self
            .0
            .max_requests_per_engine
            .get(&provider)
            .copied()
            .unwrap_or(u32::MAX);
        let views = self.0.views.lock().expect("governor views lock");
        views
            .get(&provider)
            .map(|v| v.request_count >= cap)
            .unwrap_or(false)
    }

    /// Acquire one of `provider`'s concurrency-limited permits for the
    /// whole leg that follows, however many wire requests it turns out to
    /// need. Holding the returned [`EnginePermit`] blocks any *other* leg
    /// for the *same* engine beyond that engine's own concurrency limit
    /// (#73: [`resolve_concurrency`], 1 permit = the pre-#73 "engines
    /// never overlap themselves" behavior) from acquiring until this
    /// permit is dropped; legs for a *different* engine are never blocked
    /// by this (#63: "engines still run in parallel with each other").
    /// Acquiring does not itself wait or make any HTTP request -- call
    /// [`EnginePermit::pace`] before each actual wire request.
    pub(crate) async fn acquire(&self, provider: SearchProvider) -> EnginePermit<'_> {
        let permit = self
            .0
            .queues
            .get(&provider)
            .expect("every provider has a pre-populated queue")
            .acquire()
            .await
            .expect("engine semaphore is never closed");
        EnginePermit::new(self, provider, permit)
    }

    /// A leg succeeded: clear any residual suspension (a 200 proves the
    /// wall came down; a stale duration should not keep skipping a healthy
    /// engine). No-op for a hermetic `Governor`.
    pub(crate) fn record_success(&self, provider: SearchProvider) {
        if self.0.hermetic {
            return;
        }
        self.ensure_loaded();
        let changed = {
            let mut views = self.0.views.lock().expect("governor views lock");
            match views.get_mut(&provider) {
                Some(view) if view.suspended_until.is_some() => {
                    view.suspended_until = None;
                    view.suspend_reason.clear();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.persist();
        }
    }

    /// A leg failed: suspend `provider` if `err`'s kind maps to a
    /// suspension duration (see [`suspension_for`]); a `None`/zero mapping
    /// is a no-op. Never *shortens* an existing suspension (two concurrent
    /// legs for the same engine can settle in either order; a live
    /// `Challenge` at 3600 s must not be overwritten by a slower-arriving
    /// 503 at 30 s) -- compares the new candidate expiry against the
    /// current one and keeps whichever is later. Overflow-safe: a
    /// pathological duration (e.g. a hostile `Retry-After` near `u64::MAX`
    /// seconds) that cannot be represented as an `Instant` is dropped
    /// rather than panicking. No-op for a hermetic `Governor`.
    pub(crate) fn record_failure(&self, provider: SearchProvider, err: &SearchProviderError) {
        if self.0.hermetic {
            return;
        }
        let Some(suspension) = suspension_for(err) else {
            return;
        };
        if suspension.duration.is_zero() {
            return;
        }
        let Some(candidate_until) = Instant::now().checked_add(suspension.duration) else {
            return;
        };
        self.ensure_loaded();
        let changed = {
            let mut views = self.0.views.lock().expect("governor views lock");
            let view = views.entry(provider).or_default();
            let should_replace = view
                .suspended_until
                .map(|existing| candidate_until > existing)
                .unwrap_or(true);
            if should_replace {
                view.suspended_until = Some(candidate_until);
                view.suspend_reason = suspension.reason.to_string();
            }
            should_replace
        };
        if changed {
            self.persist();
        }
    }

    /// Write the whole engine map back to `<cache_root>/engines.json`,
    /// atomically. Best-effort: a write failure never fails the search.
    /// `persist_lock` holds the snapshot-then-write pair together so two
    /// concurrent persists (typically one per engine) cannot interleave
    /// their rename and silently drop each other's update.
    fn persist(&self) {
        let Some(cache_root) = &self.0.cache_root else {
            return;
        };
        let _order = self.0.persist_lock.lock().expect("governor persist lock");
        let engines = {
            let views = self.0.views.lock().expect("governor views lock");
            ALL_PROVIDERS
                .into_iter()
                .filter_map(|provider| {
                    let view = views.get(&provider)?;
                    Some((
                        provider.id().to_string(),
                        PersistedEngine {
                            suspended_until_ms: view
                                .suspended_until
                                .map(|i| mono_to_wall(i, self.0.base_wall_ms, self.0.base_mono)),
                            suspend_reason: (!view.suspend_reason.is_empty())
                                .then(|| view.suspend_reason.clone()),
                            last_request_ms: view
                                .last_request
                                .map(|i| mono_to_wall(i, self.0.base_wall_ms, self.0.base_mono)),
                        },
                    ))
                })
                .collect::<HashMap<_, _>>()
        };
        if let Ok(bytes) = serde_json::to_vec_pretty(&PersistedState { engines }) {
            let _ = write_atomic(&cache_root.join(ENGINES_FILE), &bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::test_support::EnvGuard;
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn challenge_suspends_only_that_engine_and_expires() {
        let _guard = EnvGuard::set(&[]);
        let governor = Governor::new(None);
        assert!(governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .is_none());
        assert!(governor
            .suspended_remaining(SearchProvider::Startpage)
            .is_none());

        let err = SearchProviderError::Challenge {
            provider: SearchProvider::DuckDuckGo,
            detail: "walled".to_string(),
        };
        governor.record_failure(SearchProvider::DuckDuckGo, &err);

        let (remaining, reason) = governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .expect("duckduckgo suspended");
        assert_eq!(reason, "challenge");
        assert!(
            remaining > 3500 && remaining <= 3600,
            "expected ~3600s default, got {remaining}"
        );
        assert!(
            governor
                .suspended_remaining(SearchProvider::Startpage)
                .is_none(),
            "startpage must stay unaffected"
        );

        tokio::time::advance(Duration::from_secs(3601)).await;
        assert!(
            governor
                .suspended_remaining(SearchProvider::DuckDuckGo)
                .is_none(),
            "suspension must expire"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_wins_over_default_429_duration() {
        let _guard = EnvGuard::set(&[("SEARCH_SUSPEND_429_SECS", "180")]);
        let governor = Governor::new(None);
        let err = SearchProviderError::Upstream {
            provider: SearchProvider::DuckDuckGo,
            detail: "rate limited".to_string(),
            status: Some(429),
            retry_after_secs: Some(45),
        };
        governor.record_failure(SearchProvider::DuckDuckGo, &err);
        let (remaining, reason) = governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .expect("suspended");
        assert_eq!(reason, "retry_after");
        assert!(
            remaining <= 45,
            "Retry-After (45s) must win over the 180s default, got {remaining}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_shorter_failure_never_shortens_a_longer_active_suspension() {
        let _guard = EnvGuard::set(&[]);
        let governor = Governor::new(None);
        governor.record_failure(
            SearchProvider::DuckDuckGo,
            &SearchProviderError::Challenge {
                provider: SearchProvider::DuckDuckGo,
                detail: "walled".to_string(),
            },
        );
        let (long_remaining, _) = governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .expect("challenge suspended");
        governor.record_failure(
            SearchProvider::DuckDuckGo,
            &SearchProviderError::Upstream {
                provider: SearchProvider::DuckDuckGo,
                detail: "boom".to_string(),
                status: Some(500),
                retry_after_secs: None,
            },
        );
        let (remaining, reason) = governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .expect("still suspended");
        assert_eq!(
            reason, "challenge",
            "a later, shorter 500 suspension must not overwrite the longer challenge one"
        );
        assert!(
            remaining >= long_remaining - 1,
            "remaining time must not have shrunk to the 500's ~30s window: {remaining}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn success_clears_a_residual_suspension() {
        let _guard = EnvGuard::set(&[]);
        let governor = Governor::new(None);
        governor.record_failure(
            SearchProvider::Startpage,
            &SearchProviderError::Challenge {
                provider: SearchProvider::Startpage,
                detail: "walled".to_string(),
            },
        );
        assert!(governor
            .suspended_remaining(SearchProvider::Startpage)
            .is_some());
        governor.record_success(SearchProvider::Startpage);
        assert!(governor
            .suspended_remaining(SearchProvider::Startpage)
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_never_suspends_by_default() {
        let _guard = EnvGuard::set(&[]);
        let governor = Governor::new(None);
        governor.record_failure(
            SearchProvider::DuckDuckGo,
            &SearchProviderError::Timeout {
                provider: SearchProvider::DuckDuckGo,
                query: "q".to_string(),
            },
        );
        assert!(governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_suspension_recorded_while_a_leg_waits_is_visible_after_acquire() {
        // The suspension check before acquiring the queue is not the only
        // one that matters: a leg already queued behind another same-
        // engine leg must not dispatch once that other leg has just
        // suspended the engine. Governor::acquire itself does not
        // recheck (that is fanout.rs's job, right after acquiring); this
        // test proves the state acquire/pace would observe is correct.
        let _guard = EnvGuard::set(&[]);
        let governor = Governor::new(None);
        {
            let _first = governor.acquire(SearchProvider::DuckDuckGo).await;
            governor.record_failure(
                SearchProvider::DuckDuckGo,
                &SearchProviderError::Challenge {
                    provider: SearchProvider::DuckDuckGo,
                    detail: "walled".to_string(),
                },
            );
        }
        assert!(
            governor
                .suspended_remaining(SearchProvider::DuckDuckGo)
                .is_some(),
            "a second leg acquiring after the first releases must see the suspension"
        );
    }

    #[test]
    fn hermetic_governor_never_suspends_paces_or_caps() {
        let governor = Governor::hermetic();
        assert!(!governor.over_budget(SearchProvider::DuckDuckGo));
        governor.record_failure(
            SearchProvider::DuckDuckGo,
            &SearchProviderError::Challenge {
                provider: SearchProvider::DuckDuckGo,
                detail: "walled".to_string(),
            },
        );
        assert!(governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .is_none());
        assert_eq!(
            governor.enabled_providers(),
            vec![
                SearchProvider::Startpage,
                SearchProvider::Brave,
                SearchProvider::Yahoo,
                SearchProvider::DuckDuckGo,
                SearchProvider::Bing,
            ]
        );
    }
}
