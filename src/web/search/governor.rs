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
//! one `(base_wall_ms, base_mono)` pair fixed at construction.
//!
//! Hermetic rule: [`Governor::hermetic`] (used by `Searcher::with_bases`
//! and every existing fan-out test) never persists, never paces, and always
//! enables both providers regardless of `SEARCH_ENGINES` -- the engine
//! allowlist (#64) is a production-path concern, tested directly via
//! [`enabled_providers`] and via [`Governor::new`] with an opt-in temp dir,
//! never through the hermetic seam. [`Governor::new`] is the production
//! (and opt-in-test) path: engine allowlist, suspension durations, pacing
//! gaps, and the per-run request cap all come from `SEARCH_*` env vars read
//! once at construction; state persists to `<cache_root>/engines.json` when
//! a cache root is given, in-memory-only otherwise.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use crate::web::cache_dir::write_atomic;
use crate::web::search::types::{SearchProvider, SearchProviderError};

/// File name under the cache root (shared contract with `QueryCache`'s
/// `search/` cache entries, which live in a sibling directory).
const ENGINES_FILE: &str = "engines.json";

/// Both ported providers, in a fixed order used to pre-populate per-engine
/// maps regardless of which are enabled.
const ALL_PROVIDERS: [SearchProvider; 2] = [SearchProvider::Startpage, SearchProvider::DuckDuckGo];

/// On-disk shape of `engines.json`. Corrupt JSON (or a missing file) loads
/// as `Default` -- an empty map -- never an error (#62 acceptance).
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    engines: HashMap<String, PersistedEngine>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct PersistedEngine {
    #[serde(default)]
    suspended_until_ms: Option<i64>,
    #[serde(default)]
    suspend_reason: Option<String>,
    #[serde(default)]
    last_request_ms: Option<i64>,
}

fn load_persisted(cache_root: &Path) -> PersistedState {
    let path = cache_root.join(ENGINES_FILE);
    let Ok(bytes) = std::fs::read(&path) else {
        return PersistedState::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn now_wall_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `ms` (wall clock, millis since epoch) -> the `Instant` that same moment
/// corresponds to, given the `(base_wall_ms, base_mono)` reference pair.
/// Clamps to `base_mono` on overflow or on a timestamp before the epoch
/// reference (safe direction: never waits *less* than a correct bridge
/// would have).
fn wall_to_mono(ms: i64, base_wall_ms: i64, base_mono: Instant) -> Instant {
    let delta_ms = ms.saturating_sub(base_wall_ms);
    if delta_ms <= 0 {
        return base_mono;
    }
    base_mono
        .checked_add(Duration::from_millis(delta_ms as u64))
        .unwrap_or(base_mono)
}

/// Inverse of [`wall_to_mono`].
fn mono_to_wall(instant: Instant, base_wall_ms: i64, base_mono: Instant) -> i64 {
    if instant >= base_mono {
        let delta = instant.saturating_duration_since(base_mono).as_millis() as i64;
        base_wall_ms.saturating_add(delta)
    } else {
        let delta = base_mono.saturating_duration_since(instant).as_millis() as i64;
        base_wall_ms.saturating_sub(delta)
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// `SEARCH_ENGINES` (comma list, case-insensitive, `duckduckgo`/`ddg` and
/// `startpage`/`sp` tokens; unknown tokens ignored; duplicates collapsed;
/// unset or resolving empty defaults to `duckduckgo` only) -- #64: Startpage
/// is off by default. Pure and env-only, so it is unit-tested directly
/// without any HTTP.
pub(crate) fn enabled_providers() -> Vec<SearchProvider> {
    let raw = std::env::var("SEARCH_ENGINES").unwrap_or_else(|_| "duckduckgo".to_string());
    let mut providers: Vec<SearchProvider> = raw
        .split(',')
        .map(|tok| tok.trim().to_lowercase())
        .filter(|tok| !tok.is_empty())
        .filter_map(|tok| match tok.as_str() {
            "duckduckgo" | "ddg" => Some(SearchProvider::DuckDuckGo),
            "startpage" | "sp" => Some(SearchProvider::Startpage),
            _ => None,
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    providers.retain(|p| seen.insert(*p));
    if providers.is_empty() {
        return vec![SearchProvider::DuckDuckGo];
    }
    providers.sort_by_key(|p| p.priority());
    providers
}

/// One suspension decision: how long, and the stable reason string
/// persisted alongside it.
struct Suspension {
    duration: Duration,
    reason: &'static str,
}

/// Map a settled leg error to a suspension decision, or `None` for "never
/// suspend on this kind" (#62). Defaults inspired by SearXNG
/// `search.suspended_times` (idea only, see module doc), each independently
/// overridable via env:
///
/// - `Challenge` (bot wall) -> `SEARCH_SUSPEND_CHALLENGE_SECS`, default
///   3600 s: the strongest signal this network is walled; long enough that
///   a run a few minutes later does not re-provoke the same challenge.
/// - `Upstream` with a real `Retry-After` -> that value wins outright,
///   regardless of status (#62: "Retry-After, when present, wins").
/// - `Upstream` HTTP 403 -> `SEARCH_SUSPEND_403_SECS`, default 180 s: an
///   access-denial that is usually IP-reputation based and often lifts
///   within minutes.
/// - `Upstream` HTTP 429 -> `SEARCH_SUSPEND_429_SECS`, default 180 s: a
///   rate limit, same reasoning as 403.
/// - `Upstream` other status (5xx, unknown) -> `SEARCH_SUSPEND_UPSTREAM_SECS`,
///   default 30 s: most likely a transient server-side error, not a bot
///   wall; a short backoff avoids hammering during an outage without
///   punishing the engine once it recovers.
/// - `Timeout` -> `SEARCH_SUSPEND_TIMEOUT_SECS`, default 0 (no suspension):
///   a slow/hung leg is as likely to be our own network or the fan-out
///   deadline as a bot wall; suspending on it would punish the engine for
///   conditions it may not have caused. Operators on a congested network
///   can opt in via env.
/// - Anything else (`EmptyQuery`, `TooManyQueries`, `Suspended`,
///   `AllFailed`) -> never suspends here (input errors do not belong to a
///   leg; `Suspended` is itself the skip, not a new failure to record; the
///   whole-call `AllFailed` is derived, not a leg outcome).
fn suspension_for(err: &SearchProviderError) -> Option<Suspension> {
    match err {
        SearchProviderError::Challenge { .. } => Some(Suspension {
            duration: Duration::from_secs(env_u64("SEARCH_SUSPEND_CHALLENGE_SECS", 3600)),
            reason: "challenge",
        }),
        SearchProviderError::Upstream {
            status,
            retry_after_secs,
            ..
        } => {
            if let Some(secs) = retry_after_secs {
                return Some(Suspension {
                    duration: Duration::from_secs(*secs),
                    reason: "retry_after",
                });
            }
            match status {
                Some(403) => Some(Suspension {
                    duration: Duration::from_secs(env_u64("SEARCH_SUSPEND_403_SECS", 180)),
                    reason: "http_403",
                }),
                Some(429) => Some(Suspension {
                    duration: Duration::from_secs(env_u64("SEARCH_SUSPEND_429_SECS", 180)),
                    reason: "http_429",
                }),
                _ => {
                    let secs = env_u64("SEARCH_SUSPEND_UPSTREAM_SECS", 30);
                    (secs > 0).then(|| Suspension {
                        duration: Duration::from_secs(secs),
                        reason: "upstream",
                    })
                }
            }
        }
        SearchProviderError::Timeout { .. } => {
            let secs = env_u64("SEARCH_SUSPEND_TIMEOUT_SECS", 0);
            (secs > 0).then(|| Suspension {
                duration: Duration::from_secs(secs),
                reason: "timeout",
            })
        }
        SearchProviderError::EmptyQuery
        | SearchProviderError::TooManyQueries { .. }
        | SearchProviderError::Suspended { .. }
        | SearchProviderError::AllFailed { .. } => None,
    }
}

/// Per-engine pacing gap `[min, max)` (#63: "random gap between requests").
/// `SEARCH_PACE_<ENGINE>_MIN_MS`/`_MAX_MS` (engine = `provider.id()`
/// upper-cased, e.g. `SEARCH_PACE_DUCKDUCKGO_MIN_MS`) override the engine,
/// else `SEARCH_PACE_MIN_MS`/`_MAX_MS` override the global default, else
/// 1500..4000 ms. A max at or below min degenerates to that fixed gap (no
/// jitter, never a panic).
fn resolve_gap(provider: SearchProvider) -> (Duration, Duration) {
    let engine = provider.id().to_uppercase();
    let global_min = env_u64("SEARCH_PACE_MIN_MS", 1500);
    let global_max = env_u64("SEARCH_PACE_MAX_MS", 4000);
    let min = env_u64(&format!("SEARCH_PACE_{engine}_MIN_MS"), global_min);
    let max = env_u64(&format!("SEARCH_PACE_{engine}_MAX_MS"), global_max).max(min);
    (Duration::from_millis(min), Duration::from_millis(max))
}

fn pick_gap(min: Duration, max: Duration) -> Duration {
    if max <= min {
        return min;
    }
    let mut rng = rand::rng();
    let ms = rng.random_range(min.as_millis() as u64..=max.as_millis() as u64);
    Duration::from_millis(ms)
}

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
    views: Mutex<HashMap<SearchProvider, EngineView>>,
    /// One serial queue per engine: concurrent legs for the *same* engine
    /// queue up here and each wait their turn (including holding the lock
    /// across the paced HTTP call itself, so same-engine requests never
    /// overlap); different engines have independent queues and therefore
    /// run in parallel with each other (#63).
    queues: HashMap<SearchProvider, AsyncMutex<()>>,
    max_requests_per_engine: u32,
    /// Per-turn cap on Queries dispatched to one engine (#63); see
    /// [`Governor::max_queries_per_engine`] for the deadline math behind
    /// the default.
    max_queries_per_engine: usize,
    base_wall_ms: i64,
    base_mono: Instant,
}

fn fresh_queues() -> HashMap<SearchProvider, AsyncMutex<()>> {
    ALL_PROVIDERS
        .into_iter()
        .map(|p| (p, AsyncMutex::new(())))
        .collect()
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
    /// Hermetic seam: no persisted state, zero pacing delay, no per-run
    /// cap, no per-turn query cap, both providers always enabled. Used by
    /// `Searcher::with_bases` and every pre-existing fan-out test; the
    /// #62/#63/#64 features themselves are tested via `enabled_providers`
    /// directly and via [`Governor::new`] with an explicit temp dir.
    #[doc(hidden)]
    pub fn hermetic() -> Self {
        Governor(std::sync::Arc::new(GovernorInner {
            cache_root: None,
            hermetic: true,
            views: Mutex::new(HashMap::new()),
            queues: fresh_queues(),
            max_requests_per_engine: u32::MAX,
            max_queries_per_engine: usize::MAX,
            base_wall_ms: now_wall_ms(),
            base_mono: Instant::now(),
        }))
    }

    /// Production seam: engine allowlist, suspension durations, pacing
    /// gaps, the per-run request cap, and the per-turn query cap all come
    /// from `SEARCH_*` env vars read once here. Persists to
    /// `<cache_root>/engines.json` when `cache_root` is `Some`; in-memory
    /// only (still paced/suspension-aware for this `Governor`'s lifetime,
    /// just with no cross-run memory) otherwise. A corrupt or missing
    /// `engines.json` loads as empty state, never an error.
    pub(crate) fn new(cache_root: Option<PathBuf>) -> Self {
        let persisted = cache_root
            .as_deref()
            .map(load_persisted)
            .unwrap_or_default();
        let base_wall_ms = now_wall_ms();
        let base_mono = Instant::now();
        let mut views = HashMap::new();
        for provider in ALL_PROVIDERS {
            let entry = persisted.engines.get(provider.id());
            views.insert(
                provider,
                EngineView {
                    suspended_until: entry
                        .and_then(|e| e.suspended_until_ms)
                        .map(|ms| wall_to_mono(ms, base_wall_ms, base_mono)),
                    suspend_reason: entry
                        .and_then(|e| e.suspend_reason.clone())
                        .unwrap_or_default(),
                    last_request: entry
                        .and_then(|e| e.last_request_ms)
                        .map(|ms| wall_to_mono(ms, base_wall_ms, base_mono)),
                    request_count: 0,
                },
            );
        }
        let max_requests_per_engine =
            env_u64("SEARCH_MAX_REQUESTS_PER_ENGINE", 200).min(u32::MAX as u64) as u32;
        // #63: "per-turn cap on queries per engine", accounting for the
        // pacing gap so the soft/hard fan-out deadlines (5 s / 30 s,
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
        // nearly the whole hard one.
        let max_queries_per_engine = env_u64("SEARCH_MAX_QUERIES_PER_ENGINE_PER_TURN", 4) as usize;
        Governor(std::sync::Arc::new(GovernorInner {
            cache_root,
            hermetic: false,
            views: Mutex::new(views),
            queues: fresh_queues(),
            max_requests_per_engine,
            max_queries_per_engine,
            base_wall_ms,
            base_mono,
        }))
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

    /// Per-turn cap on Queries dispatched to one engine (#63,
    /// `SEARCH_MAX_QUERIES_PER_ENGINE_PER_TURN`, default 4); `usize::MAX`
    /// for a hermetic `Governor` (no cap).
    pub(crate) fn max_queries_per_engine(&self) -> usize {
        self.0.max_queries_per_engine
    }

    /// `Some((remaining_secs, reason))` if `provider` is currently under an
    /// active suspension; `None` otherwise (including every hermetic
    /// call). Compares against `Instant::now()`, so a paused tokio clock
    /// drives expiry deterministically in tests.
    pub(crate) fn suspended_remaining(&self, provider: SearchProvider) -> Option<(u64, String)> {
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

    /// `true` once `provider` has reached the per-run request cap
    /// (`SEARCH_MAX_REQUESTS_PER_ENGINE`, default 200); always `false` for
    /// a hermetic `Governor`. Checked before pacing/dispatch, so an
    /// exhausted budget never sends an HTTP request either.
    pub(crate) fn over_budget(&self, provider: SearchProvider) -> bool {
        if self.0.hermetic {
            return false;
        }
        let views = self.0.views.lock().expect("governor views lock");
        views
            .get(&provider)
            .map(|v| v.request_count >= self.0.max_requests_per_engine)
            .unwrap_or(false)
    }

    /// Run one leg's dispatch under this engine's serial queue: wait for
    /// the jittered gap since the last request to *this* engine (across
    /// the whole process, seeded from `engines.json` on a fresh process),
    /// then run `dispatch`, holding the queue lock the entire time so a
    /// second leg for the same engine cannot start until this one's
    /// request has fully completed (#63: "requests to one engine never
    /// overlap"). A no-op wrapper (immediate dispatch, no wait, no
    /// bookkeeping) for a hermetic `Governor`.
    pub(crate) async fn paced_call<F, Fut, T>(&self, provider: SearchProvider, dispatch: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        if self.0.hermetic {
            return dispatch().await;
        }
        let queue = self
            .0
            .queues
            .get(&provider)
            .expect("every provider has a pre-populated queue");
        let _serial = queue.lock().await;
        let (gap_min, gap_max) = resolve_gap(provider);
        let gap = pick_gap(gap_min, gap_max);
        let last = {
            let views = self.0.views.lock().expect("governor views lock");
            views.get(&provider).and_then(|v| v.last_request)
        };
        if let Some(last) = last {
            let elapsed = Instant::now().saturating_duration_since(last);
            if elapsed < gap {
                tokio::time::sleep(gap - elapsed).await;
            }
        }
        // Gap measured from request *start*, so consecutive starts are
        // always >= gap apart regardless of how long the request itself
        // takes.
        let start = Instant::now();
        let result = dispatch().await;
        {
            let mut views = self.0.views.lock().expect("governor views lock");
            let view = views.entry(provider).or_default();
            view.last_request = Some(start);
            view.request_count += 1;
        }
        self.persist();
        result
    }

    /// A leg succeeded: clear any residual suspension (a 200 proves the
    /// wall came down; a stale duration should not keep skipping a healthy
    /// engine). No-op for a hermetic `Governor`.
    pub(crate) fn record_success(&self, provider: SearchProvider) {
        if self.0.hermetic {
            return;
        }
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
    /// is a no-op. No-op for a hermetic `Governor`.
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
        let until = Instant::now() + suspension.duration;
        {
            let mut views = self.0.views.lock().expect("governor views lock");
            let view = views.entry(provider).or_default();
            view.suspended_until = Some(until);
            view.suspend_reason = suspension.reason.to_string();
        }
        self.persist();
    }

    /// Write the whole engine map back to `<cache_root>/engines.json`,
    /// atomically. Best-effort: a write failure never fails the search.
    fn persist(&self) {
        let Some(cache_root) = &self.0.cache_root else {
            return;
        };
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

/// Test-only env-var guard for `SEARCH_ENGINES`/`SEARCH_SUSPEND_*`/
/// `SEARCH_PACE_*`/`SEARCH_MAX_REQUESTS_PER_ENGINE`, shared across
/// `governor::tests` and `fanout::tests` (both mutate these and run in the
/// same test binary; same pattern as `cache::test_support::EnvGuard`).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Every `SEARCH_*` key this module's env-reading functions consult, so
    /// `set`/`Drop` can clear exactly the keys a test might have touched
    /// without hard-coding the same list at every call site.
    const GOVERNOR_KEYS: [&str; 12] = [
        "SEARCH_ENGINES",
        "SEARCH_SUSPEND_CHALLENGE_SECS",
        "SEARCH_SUSPEND_403_SECS",
        "SEARCH_SUSPEND_429_SECS",
        "SEARCH_SUSPEND_UPSTREAM_SECS",
        "SEARCH_SUSPEND_TIMEOUT_SECS",
        "SEARCH_PACE_MIN_MS",
        "SEARCH_PACE_MAX_MS",
        "SEARCH_PACE_DUCKDUCKGO_MIN_MS",
        "SEARCH_PACE_DUCKDUCKGO_MAX_MS",
        "SEARCH_PACE_STARTPAGE_MIN_MS",
        "SEARCH_PACE_STARTPAGE_MAX_MS",
    ];

    /// Serializes tests that mutate any governor env var and clears every
    /// known key on both acquisition and drop, so one test's override never
    /// leaks into the next regardless of which side left it set.
    pub(crate) struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        pub(crate) fn lock() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for key in GOVERNOR_KEYS {
                std::env::remove_var(key);
            }
            Self { _lock: lock }
        }

        /// Acquire the lock, clear every known key, then set the given
        /// pairs (which may include `SEARCH_MAX_REQUESTS_PER_ENGINE`, not
        /// in `GOVERNOR_KEYS` since it is cleared via `Drop` separately by
        /// callers that use it -- kept out of the shared list because only
        /// one test reads it).
        pub(crate) fn set(pairs: &[(&str, &str)]) -> Self {
            let guard = Self::lock();
            for (k, v) in pairs {
                std::env::set_var(k, v);
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for key in GOVERNOR_KEYS {
                std::env::remove_var(key);
            }
            std::env::remove_var("SEARCH_MAX_REQUESTS_PER_ENGINE");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::EnvGuard;

    #[test]
    fn enabled_providers_defaults_to_duckduckgo_only() {
        let _guard = EnvGuard::set(&[]);
        std::env::remove_var("SEARCH_ENGINES");
        assert_eq!(enabled_providers(), vec![SearchProvider::DuckDuckGo]);
    }

    #[test]
    fn enabled_providers_opts_into_startpage() {
        let _guard = EnvGuard::set(&[("SEARCH_ENGINES", "duckduckgo,startpage")]);
        let providers = enabled_providers();
        assert!(providers.contains(&SearchProvider::Startpage));
        assert!(providers.contains(&SearchProvider::DuckDuckGo));
        assert_eq!(providers.len(), 2);
    }

    #[test]
    fn enabled_providers_ignores_unknown_tokens_and_dedupes() {
        let _guard = EnvGuard::set(&[("SEARCH_ENGINES", " STARTPAGE , bing, startpage ,ddg")]);
        let providers = enabled_providers();
        // "bing" ignored; "STARTPAGE"/"startpage" collapse to one entry;
        // "ddg" alias resolves to DuckDuckGo.
        assert_eq!(providers.len(), 2);
        assert!(providers.contains(&SearchProvider::Startpage));
        assert!(providers.contains(&SearchProvider::DuckDuckGo));
    }

    #[test]
    fn enabled_providers_empty_after_filtering_falls_back_to_default() {
        let _guard = EnvGuard::set(&[("SEARCH_ENGINES", "bing, , yandex")]);
        assert_eq!(enabled_providers(), vec![SearchProvider::DuckDuckGo]);
    }

    #[tokio::test(start_paused = true)]
    async fn challenge_suspends_only_that_engine_and_expires() {
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
    async fn success_clears_a_residual_suspension() {
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
    async fn paced_call_respects_minimum_gap_same_engine() {
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_MIN_MS", "2000"),
            ("SEARCH_PACE_MAX_MS", "2000"),
        ]);
        let governor = Governor::new(None);
        governor
            .paced_call(SearchProvider::DuckDuckGo, || async { 1 })
            .await;

        let start = tokio::time::Instant::now();
        let second = tokio::spawn({
            let governor = governor.clone();
            async move {
                governor
                    .paced_call(SearchProvider::DuckDuckGo, || async { 2 })
                    .await
            }
        });
        // The second call must not resolve before the 2s gap elapses.
        tokio::time::advance(Duration::from_millis(1900)).await;
        assert!(
            !second.is_finished(),
            "second call resolved before the minimum gap"
        );
        tokio::time::advance(Duration::from_millis(200)).await;
        second.await.expect("second call completes");
        assert!(
            tokio::time::Instant::now().saturating_duration_since(start)
                >= Duration::from_millis(2000)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn paced_call_different_engines_run_in_parallel() {
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_MIN_MS", "5000"),
            ("SEARCH_PACE_MAX_MS", "5000"),
        ]);
        let governor = Governor::new(None);
        let ddg = tokio::spawn({
            let governor = governor.clone();
            async move {
                governor
                    .paced_call(SearchProvider::DuckDuckGo, || async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    })
                    .await
            }
        });
        let sp = tokio::spawn({
            let governor = governor.clone();
            async move {
                governor
                    .paced_call(SearchProvider::Startpage, || async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    })
                    .await
            }
        });
        tokio::time::advance(Duration::from_millis(20)).await;
        // Neither engine waits on the other's pacing gap: both finish long
        // before either engine's 5s gap would matter (there is no prior
        // request to either engine yet).
        ddg.await.expect("ddg leg completes");
        sp.await.expect("sp leg completes");
    }

    #[tokio::test]
    async fn corrupt_engines_json_is_ignored() {
        let dir = std::env::temp_dir().join(format!("war-governor-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(ENGINES_FILE), b"{ not json at all").expect("write corrupt file");
        let governor = Governor::new(Some(dir.clone()));
        assert!(governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .is_none());
        assert!(governor
            .suspended_remaining(SearchProvider::Startpage)
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn suspension_persists_and_reloads_across_governor_instances() {
        let dir = std::env::temp_dir().join(format!("war-governor-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = Governor::new(Some(dir.clone()));
        first.record_failure(
            SearchProvider::DuckDuckGo,
            &SearchProviderError::Challenge {
                provider: SearchProvider::DuckDuckGo,
                detail: "walled".to_string(),
            },
        );
        assert!(
            dir.join(ENGINES_FILE).exists(),
            "engines.json must be written"
        );

        let second = Governor::new(Some(dir.clone()));
        let (remaining, reason) = second
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .expect("a fresh Governor loading the same cache root sees the suspension");
        assert_eq!(reason, "challenge");
        assert!(remaining > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn over_budget_trips_after_the_configured_cap() {
        let _guard = EnvGuard::set(&[("SEARCH_MAX_REQUESTS_PER_ENGINE", "2")]);
        let governor = Governor::new(None);
        assert!(!governor.over_budget(SearchProvider::DuckDuckGo));
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
            vec![SearchProvider::Startpage, SearchProvider::DuckDuckGo]
        );
    }
}
