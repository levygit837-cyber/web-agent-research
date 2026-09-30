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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::web::cache_dir::write_atomic;
use crate::web::search::types::{SearchProvider, SearchProviderError};

/// File name under the cache root (shared contract with `QueryCache`'s
/// `search/` cache entries, which live in a sibling directory).
const ENGINES_FILE: &str = "engines.json";

/// Every ported provider, in a fixed order used to pre-populate per-engine
/// maps regardless of which are enabled.
const ALL_PROVIDERS: [SearchProvider; 5] = [
    SearchProvider::Startpage,
    SearchProvider::Brave,
    SearchProvider::Yahoo,
    SearchProvider::DuckDuckGo,
    SearchProvider::Bing,
];

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
/// `ms` at or after `base_wall_ms` (the common case for a still-active
/// suspension, or a future timestamp from clock skew) offsets forward from
/// `base_mono`, clamping to `base_mono` on the (pathological) overflow case.
/// `ms` before `base_wall_ms` (the common case for `last_request_ms`, which
/// is always in the past by construction, and for an already-expired
/// suspension) offsets *backward* from `base_mono` by the real elapsed wall
/// time, so callers see the true historical gap instead of "just now" --
/// `None` when that offset underflows `Instant` (a timestamp far enough in
/// the past that no realistic pacing gap or suspension window could still
/// apply to it), which callers treat as "no residual state", the same
/// outcome an accurate arbitrary-precision clock would produce.
fn wall_to_mono(ms: i64, base_wall_ms: i64, base_mono: Instant) -> Option<Instant> {
    let delta_ms = ms.saturating_sub(base_wall_ms);
    if delta_ms >= 0 {
        Some(
            base_mono
                .checked_add(Duration::from_millis(delta_ms as u64))
                .unwrap_or(base_mono),
        )
    } else {
        base_mono.checked_sub(Duration::from_millis((-delta_ms) as u64))
    }
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

/// `key` parsed as `u64` if set and valid; `None` if unset or unparseable.
/// Shared by every 3-tier resolver below (engine-env > global-env >
/// per-engine hardcoded default): each tier is itself an `Option`, so the
/// resolver can chain `.or(...)` down to its final hardcoded fallback.
fn env_u64_opt(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_u64_opt(key).unwrap_or(default)
}

/// `SEARCH_ENGINES` (comma list, case-insensitive; `duckduckgo`/`ddg`,
/// `startpage`/`sp`, `brave`, `yahoo`, `bing` tokens; unknown tokens
/// ignored; duplicates collapsed; unset or resolving empty defaults to
/// `duckduckgo,brave,yahoo,bing` -- #64/#66: Startpage stays opt-in only
/// (it serves an Anubis proof-of-work challenge to bot-detected clients).
/// Pure and env-only, so it is unit-tested directly without any HTTP.
pub(crate) fn enabled_providers() -> Vec<SearchProvider> {
    let raw = std::env::var("SEARCH_ENGINES")
        .unwrap_or_else(|_| "duckduckgo,brave,yahoo,bing".to_string());
    let mut providers: Vec<SearchProvider> = raw
        .split(',')
        .map(|tok| tok.trim().to_lowercase())
        .filter(|tok| !tok.is_empty())
        .filter_map(|tok| match tok.as_str() {
            "duckduckgo" | "ddg" => Some(SearchProvider::DuckDuckGo),
            "startpage" | "sp" => Some(SearchProvider::Startpage),
            "brave" => Some(SearchProvider::Brave),
            "yahoo" => Some(SearchProvider::Yahoo),
            "bing" => Some(SearchProvider::Bing),
            _ => None,
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    providers.retain(|p| seen.insert(*p));
    if providers.is_empty() {
        return default_providers();
    }
    providers.sort_by_key(|p| p.priority());
    providers
}

/// The default `SEARCH_ENGINES` set, pre-sorted by priority: unset or an
/// all-unknown-tokens `SEARCH_ENGINES` both resolve here.
fn default_providers() -> Vec<SearchProvider> {
    let mut providers = vec![
        SearchProvider::DuckDuckGo,
        SearchProvider::Brave,
        SearchProvider::Yahoo,
        SearchProvider::Bing,
    ];
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
///   `Throttled`, `AllFailed`) -> never suspends here (input errors do not
///   belong to a leg; `Suspended`/`Throttled` are themselves a skip, not a
///   new failure to record; the whole-call `AllFailed` is derived, not a
///   leg outcome).
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
        | SearchProviderError::Throttled { .. }
        | SearchProviderError::AllFailed { .. } => None,
    }
}

/// Per-engine hardcoded pacing-gap default `[min, max]` ms (#73, live-
/// measured 2026-09-29, `docs/research/search-engines.md`). Bing and
/// Yahoo cleared every step of an 8s->4s->2s->1s gap step-down plus a 20+
/// request burst at 1s with zero blocks, so their default tightens to
/// 1000..2000 ms -- a conservative margin below the measured "safe at
/// <=1s" floor, not the floor itself. Brave and DuckDuckGo never reached
/// a genuine gap-tolerance measurement (Brave blocked on the very first
/// concurrency probe, before the step-down began; DuckDuckGo's
/// conservative pass used a fixed 8s gap throughout, per the issue's own
/// instruction, never stepped down) -- their default stays the original
/// pre-#73 1500..4000 ms rather than inventing a number from no evidence.
/// Startpage is untouched (out of #73's scope; unmeasured this round).
fn engine_gap_default_ms(provider: SearchProvider) -> (u64, u64) {
    match provider {
        SearchProvider::Bing | SearchProvider::Yahoo => (1000, 2000),
        SearchProvider::Brave | SearchProvider::DuckDuckGo | SearchProvider::Startpage => {
            (1500, 4000)
        }
    }
}

/// Per-engine pacing gap `[min, max]` (#63: "random gap between requests").
/// `SEARCH_PACE_<ENGINE>_MIN_MS`/`_MAX_MS` (engine = `provider.id()`
/// upper-cased, e.g. `SEARCH_PACE_DUCKDUCKGO_MIN_MS`) override the engine,
/// else `SEARCH_PACE_MIN_MS`/`_MAX_MS` override every engine uniformly
/// when set, else [`engine_gap_default_ms`]'s per-engine hardcoded
/// default (#73). A max at or below min degenerates to that fixed gap
/// (no jitter, never a panic).
fn resolve_gap(provider: SearchProvider) -> (Duration, Duration) {
    let engine = provider.id().to_uppercase();
    let (hardcoded_min, hardcoded_max) = engine_gap_default_ms(provider);
    let global_min = env_u64("SEARCH_PACE_MIN_MS", hardcoded_min);
    let global_max = env_u64("SEARCH_PACE_MAX_MS", hardcoded_max);
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

/// Per-engine hardcoded pagination-depth default (#73, live-measured
/// 2026-09-29, `docs/research/search-engines.md`). Yahoo genuinely
/// paginates (`b=`/`pz=`, verified 0% URL overlap across pages 1/3/4/5
/// this session with no sign of exhaustion at depth 5) -- default 3 is a
/// conservative margin below that measured depth, not the ceiling
/// itself. DuckDuckGo already paginates internally via its own `s`/`vqd`
/// continuation (pre-#73, unmeasured this round); its depth stays
/// unbounded in practice (bounded by `MAX_NUM_RESULTS`/a missing
/// continuation form) except for a generous safety cap of 5 pages
/// against a pathological continuation cycle -- not a behavior change
/// for any query this repo has ever seen paginate more than 2 pages.
/// Bing and Brave both stay page-1-only: Bing's `first=` pagination is
/// confirmed dead without JS (pages 1-4 byte-identical), and Brave's is
/// unmeasured (blocked on the concurrency probe before reaching it) --
/// neither ships pagination code, so this default has no effect for
/// them. Startpage is untouched (out of #73's scope; unmeasured this
/// round).
fn engine_max_pages_default(provider: SearchProvider) -> u64 {
    match provider {
        SearchProvider::Yahoo => 3,
        SearchProvider::DuckDuckGo => 5,
        SearchProvider::Bing | SearchProvider::Brave | SearchProvider::Startpage => 1,
    }
}

/// Per-engine pagination depth (#73). `SEARCH_MAX_PAGES_<ENGINE>`
/// overrides the engine; `SEARCH_MAX_PAGES` overrides every engine
/// uniformly when set; else [`engine_max_pages_default`]. Read fresh per
/// call (like [`resolve_gap`]), not cached at construction, since a
/// pagination loop reads it once at the start of one leg, not
/// continuously. Always at least 1 (a depth of 0 would fetch nothing).
pub(crate) fn resolve_max_pages(provider: SearchProvider) -> usize {
    let engine = provider.id().to_uppercase();
    let hardcoded = engine_max_pages_default(provider);
    let global = env_u64("SEARCH_MAX_PAGES", hardcoded);
    env_u64(&format!("SEARCH_MAX_PAGES_{engine}"), global).max(1) as usize
}

/// Per-engine default concurrency (#73, live-measured 2026-09-29): how
/// many requests to one engine may be genuinely in flight at once. Bing
/// and Yahoo tolerated 2 simultaneous requests with zero blocks (the
/// concurrency probe); Brave showed zero burst tolerance (blocked on
/// that very probe) and DuckDuckGo's known suspension history on this IP
/// argues for the same conservative posture even though its single pass
/// never tested concurrency directly. Startpage is untouched (out of
/// #73's scope; unmeasured this round).
fn engine_concurrency_default(provider: SearchProvider) -> u64 {
    match provider {
        SearchProvider::Bing | SearchProvider::Yahoo => 2,
        SearchProvider::Brave | SearchProvider::DuckDuckGo | SearchProvider::Startpage => 1,
    }
}

/// Resolve one engine's concurrency limit: `SEARCH_CONCURRENCY_<ENGINE>`
/// overrides the engine; `SEARCH_CONCURRENCY` overrides every engine
/// uniformly when set; else [`engine_concurrency_default`]. Read once, at
/// [`Governor::new`] construction (sizes each engine's `Semaphore`), not
/// re-read per call -- unlike the gap/suspension resolvers, a live
/// `Governor`'s concurrency limit cannot change after its queues are
/// built without adding/removing `Semaphore` permits at runtime, which no
/// caller needs.
fn resolve_concurrency(provider: SearchProvider) -> usize {
    let engine = provider.id().to_uppercase();
    let hardcoded = engine_concurrency_default(provider);
    let global = env_u64("SEARCH_CONCURRENCY", hardcoded);
    env_u64(&format!("SEARCH_CONCURRENCY_{engine}"), global).max(1) as usize
}

/// Per-engine hardcoded per-run request-cap default (#73, live-measured
/// 2026-09-29). Bing and Yahoo measured a clean 20+ request burst with
/// zero blocks, keeping the original generous 200 default. Brave
/// (blocked on request #1, zero measured burst tolerance) and DuckDuckGo
/// (blocked on request #2 of a conservative single pass, consistent with
/// its documented 1h+ suspension history on this IP) both tighten to a
/// low circuit-breaker default of 10: a conservative margin far below any
/// measured tolerance (neither has one), so a run that somehow keeps
/// queueing legs for an already-flaky engine stops well short of
/// hammering it, on top of (not instead of) the existing suspension that
/// already skips a Challenged engine outright. Startpage is untouched
/// (out of #73's scope; unmeasured this round).
fn engine_cap_default(provider: SearchProvider) -> u32 {
    match provider {
        SearchProvider::Bing | SearchProvider::Yahoo | SearchProvider::Startpage => 200,
        SearchProvider::Brave | SearchProvider::DuckDuckGo => 10,
    }
}

/// Resolve every provider's per-run request cap once, at construction
/// (the module doc: unlike suspension/gap, both request caps are read
/// once, not re-read per call). `SEARCH_MAX_REQUESTS_PER_ENGINE_<ENGINE>`
/// overrides the engine; `SEARCH_MAX_REQUESTS_PER_ENGINE` (global,
/// pre-#73) overrides every engine uniformly when set; else
/// [`engine_cap_default`].
fn resolve_caps_per_engine() -> HashMap<SearchProvider, u32> {
    ALL_PROVIDERS
        .into_iter()
        .map(|provider| {
            let engine = provider.id().to_uppercase();
            let hardcoded = engine_cap_default(provider);
            let global = env_u64("SEARCH_MAX_REQUESTS_PER_ENGINE", hardcoded as u64);
            let cap = env_u64(&format!("SEARCH_MAX_REQUESTS_PER_ENGINE_{engine}"), global)
                .min(u32::MAX as u64) as u32;
            (provider, cap)
        })
        .collect()
}

/// Resolve every provider's per-call query cap once, at construction
/// (same "read once" reasoning as [`resolve_caps_per_engine`]). No #73
/// measurement argues for a different default per engine (this caps
/// Queries per fan-out *call*, not requests per run), so every engine
/// keeps the pre-#73 default of 4;
/// `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL_<ENGINE>` overrides the
/// engine, `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL` (global, pre-#73)
/// overrides every engine uniformly when set.
fn resolve_max_queries_per_engine() -> HashMap<SearchProvider, usize> {
    ALL_PROVIDERS
        .into_iter()
        .map(|provider| {
            let engine = provider.id().to_uppercase();
            let global = env_u64("SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL", 4);
            let cap = env_u64(
                &format!("SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL_{engine}"),
                global,
            ) as usize;
            (provider, cap)
        })
        .collect()
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
        EnginePermit {
            governor: self,
            provider,
            _permit: permit,
        }
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

/// A held per-engine concurrency permit spanning one entire leg, however
/// many wire requests it turns out to need (DDG pagination, Yahoo's
/// paginated pages). Returned by [`Governor::acquire`]; dropping it frees
/// the permit for the next queued leg on the same engine, once that
/// engine's own concurrency limit (#73) allows it.
pub(crate) struct EnginePermit<'g> {
    governor: &'g Governor,
    provider: SearchProvider,
    _permit: tokio::sync::SemaphorePermit<'g>,
}

/// Result of [`EnginePermit::pace`]: whether the leg may proceed to its
/// first wire request, or was skipped by a state that changed while this
/// leg's `EnginePermit` was still queued behind a sibling leg for the same
/// engine.
#[derive(Debug)]
pub(crate) enum PaceOutcome {
    /// Proceed: the gap has elapsed and the budget has been reserved.
    Proceed,
    /// A sibling leg suspended this engine while this one was queued.
    Suspended { remaining_secs: u64, reason: String },
    /// A sibling leg exhausted the per-run budget while this one was
    /// queued.
    Throttled,
}

impl EnginePermit<'_> {
    /// Recheck suspension and the per-run budget (closing the race where a
    /// sibling leg for the *same* engine settled -- and suspended the
    /// engine, or exhausted the budget -- while this leg was still queued
    /// behind it: this engine's serial queue, held by this `EnginePermit`
    /// for its whole lifetime, guarantees at most one task is ever inside
    /// this method for a given provider, so the checks below can never
    /// race with a concurrent update to the same provider's state), then
    /// wait the jittered gap since the last leg dispatched to this engine
    /// (initial value seeded from `engines.json` on a fresh process), then
    /// atomically reserve the budget and record *this* moment as the new
    /// last-request time, persisting both. Call once per leg, immediately
    /// after [`Governor::acquire`] and before the leg's first wire
    /// request. Always [`PaceOutcome::Proceed`], with zero wait, for a
    /// hermetic `Governor`.
    pub(crate) async fn pace(&self) -> PaceOutcome {
        if self.governor.0.hermetic {
            return PaceOutcome::Proceed;
        }
        self.governor.ensure_loaded();
        if let Some((remaining_secs, reason)) = self.governor.suspended_remaining(self.provider) {
            return PaceOutcome::Suspended {
                remaining_secs,
                reason,
            };
        }
        if self.governor.over_budget(self.provider) {
            return PaceOutcome::Throttled;
        }
        let (gap_min, gap_max) = resolve_gap(self.provider);
        let gap = pick_gap(gap_min, gap_max);
        let last = {
            let views = self.governor.0.views.lock().expect("governor views lock");
            views.get(&self.provider).and_then(|v| v.last_request)
        };
        if let Some(last) = last {
            let elapsed = Instant::now().saturating_duration_since(last);
            if elapsed < gap {
                tokio::time::sleep(gap - elapsed).await;
            }
        }
        // Gap measured from request *start*, so consecutive starts are
        // always >= gap apart regardless of how long the request itself
        // takes. The budget increment happens here, not after the wire
        // request resolves, so a leg aborted mid-flight by the fan-out
        // deadline still counts against the cap and still advances the
        // pacing clock -- it did send a real request.
        let now = Instant::now();
        {
            let mut views = self.governor.0.views.lock().expect("governor views lock");
            let view = views.entry(self.provider).or_default();
            view.last_request = Some(now);
            view.request_count += 1;
        }
        self.governor.persist();
        PaceOutcome::Proceed
    }
}

/// Test-only env-var guard for every `SEARCH_*` key this module's
/// env-reading functions consult, shared across `governor::tests` and
/// `fanout::tests` (both mutate these and run in the same test binary;
/// same pattern as `cache::test_support::EnvGuard`).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Every key this module's env-reading functions consult, so
    /// `set`/`Drop` can clear exactly the keys a test might have touched
    /// without hard-coding the same list at every call site.
    const GOVERNOR_KEYS: [&str; 27] = [
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
        "SEARCH_PACE_BRAVE_MIN_MS",
        "SEARCH_PACE_BRAVE_MAX_MS",
        "SEARCH_PACE_YAHOO_MIN_MS",
        "SEARCH_PACE_YAHOO_MAX_MS",
        "SEARCH_PACE_BING_MIN_MS",
        "SEARCH_PACE_BING_MAX_MS",
        "SEARCH_MAX_REQUESTS_PER_ENGINE",
        "SEARCH_MAX_REQUESTS_PER_ENGINE_BRAVE",
        "SEARCH_MAX_REQUESTS_PER_ENGINE_DUCKDUCKGO",
        "SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL",
        "SEARCH_CONCURRENCY",
        "SEARCH_CONCURRENCY_BRAVE",
        "SEARCH_MAX_PAGES",
        "SEARCH_MAX_PAGES_YAHOO",
        "SEARCH_MAX_PAGES_DUCKDUCKGO",
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
        /// pairs.
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
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;
    use test_support::EnvGuard;

    #[test]
    fn enabled_providers_defaults_to_ddg_brave_yahoo_bing() {
        let _guard = EnvGuard::set(&[]);
        std::env::remove_var("SEARCH_ENGINES");
        let providers = enabled_providers();
        assert_eq!(providers.len(), 4, "startpage stays opt-in: {providers:?}");
        assert!(!providers.contains(&SearchProvider::Startpage));
        for expected in [
            SearchProvider::DuckDuckGo,
            SearchProvider::Brave,
            SearchProvider::Yahoo,
            SearchProvider::Bing,
        ] {
            assert!(
                providers.contains(&expected),
                "missing {expected:?}: {providers:?}"
            );
        }
        // Priority order, not input/declaration order.
        assert_eq!(
            providers,
            vec![
                SearchProvider::Brave,
                SearchProvider::Yahoo,
                SearchProvider::DuckDuckGo,
                SearchProvider::Bing,
            ]
        );
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
        let _guard = EnvGuard::set(&[("SEARCH_ENGINES", " STARTPAGE , yandex, startpage ,ddg")]);
        let providers = enabled_providers();
        // "yandex" ignored (not a #66-kept engine); "STARTPAGE"/"startpage"
        // collapse to one entry; "ddg" alias resolves to DuckDuckGo.
        assert_eq!(providers.len(), 2);
        assert!(providers.contains(&SearchProvider::Startpage));
        assert!(providers.contains(&SearchProvider::DuckDuckGo));
    }

    #[test]
    fn enabled_providers_empty_after_filtering_falls_back_to_default() {
        let _guard = EnvGuard::set(&[("SEARCH_ENGINES", "yandex, , marginalia")]);
        assert_eq!(enabled_providers(), default_providers());
    }

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
    async fn acquire_pace_respects_minimum_gap_same_engine() {
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_MIN_MS", "2000"),
            ("SEARCH_PACE_MAX_MS", "2000"),
        ]);
        let governor = Governor::new(None);
        {
            let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }

        let start = tokio::time::Instant::now();
        let second = tokio::spawn({
            let governor = governor.clone();
            async move {
                let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
                assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
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
    async fn acquire_different_engines_run_in_parallel() {
        // Proof of real overlap, not just "finishes within the window": an
        // in-flight counter observes both engines' dispatches concurrently
        // in the air at once, which a single shared queue (a same-engine
        // bug) could never produce.
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_MIN_MS", "5000"),
            ("SEARCH_PACE_MAX_MS", "5000"),
        ]);
        let governor = Governor::new(None);
        let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let max_seen = std::sync::Arc::new(AtomicUsize::new(0));
        let run_leg = |provider: SearchProvider| {
            let governor = governor.clone();
            let in_flight = in_flight.clone();
            let max_seen = max_seen.clone();
            async move {
                let permit = governor.acquire(provider).await;
                assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
                let now = in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                max_seen.fetch_max(now, AtomicOrdering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
            }
        };
        let ddg = tokio::spawn(run_leg(SearchProvider::DuckDuckGo));
        let sp = tokio::spawn(run_leg(SearchProvider::Startpage));
        tokio::time::advance(Duration::from_millis(20)).await;
        ddg.await.expect("ddg leg completes");
        sp.await.expect("sp leg completes");
        assert_eq!(
            max_seen.load(AtomicOrdering::SeqCst),
            2,
            "both engines must have been in flight at the same instant"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn two_engines_with_different_gaps_pace_differently() {
        // #73 acceptance: "two engines with different gaps pace
        // differently". Pinned to fixed, non-overlapping gaps via env
        // override (rather than relying on the #73 defaults' random
        // draw, whose ranges overlap -- Bing 1000..2000ms vs Brave
        // 1500..4000ms -- and would make this assertion flaky whenever
        // Brave's draw happened to land at or below 2100ms) so a second
        // leg to each engine waits its own engine's minimum
        // deterministically, not the other's.
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_BING_MIN_MS", "1000"),
            ("SEARCH_PACE_BING_MAX_MS", "1000"),
            ("SEARCH_PACE_BRAVE_MIN_MS", "4000"),
            ("SEARCH_PACE_BRAVE_MAX_MS", "4000"),
        ]);
        let governor = Governor::new(None);
        {
            let permit = governor.acquire(SearchProvider::Bing).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }
        {
            let permit = governor.acquire(SearchProvider::Brave).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }

        let bing_second = tokio::spawn({
            let governor = governor.clone();
            async move {
                let permit = governor.acquire(SearchProvider::Bing).await;
                assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
            }
        });
        let brave_second = tokio::spawn({
            let governor = governor.clone();
            async move {
                let permit = governor.acquire(SearchProvider::Brave).await;
                assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
            }
        });

        // Just past Bing's fixed 1000ms gap: Bing's second leg must have
        // resolved by now, but Brave's fixed 4000ms gap must not have.
        tokio::time::advance(Duration::from_millis(1100)).await;
        assert!(
            bing_second.is_finished(),
            "Bing's fixed 1000ms gap must have already elapsed"
        );
        assert!(
            !brave_second.is_finished(),
            "Brave's fixed 4000ms gap must not have elapsed yet -- \
             if both paced identically this would already be finished"
        );

        tokio::time::advance(Duration::from_millis(3000)).await;
        brave_second.await.expect("brave second leg completes");
    }

    #[tokio::test(start_paused = true)]
    async fn engine_allowed_2_in_flight_overlaps_engine_allowed_1_never_does() {
        // #73 acceptance: "an engine allowed 2 in flight overlaps, while
        // an engine allowed 1 never does". Bing defaults to concurrency 2
        // (measured: zero blocks on the live 2-in-flight probe); Brave
        // defaults to 1 (measured: blocked on that exact probe). Three
        // legs to each engine, holding their permit for an overlapping
        // window, prove the difference via a live in-flight counter --
        // not just "all three eventually finish".
        let _guard = EnvGuard::set(&[("SEARCH_PACE_MIN_MS", "0"), ("SEARCH_PACE_MAX_MS", "0")]);
        let governor = Governor::new(None);

        async fn max_in_flight(
            governor: &Governor,
            provider: SearchProvider,
            legs: usize,
        ) -> usize {
            let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
            let max_seen = std::sync::Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();
            for _ in 0..legs {
                let governor = governor.clone();
                let in_flight = in_flight.clone();
                let max_seen = max_seen.clone();
                handles.push(tokio::spawn(async move {
                    let permit = governor.acquire(provider).await;
                    assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
                    let now = in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                    max_seen.fetch_max(now, AtomicOrdering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
                }));
            }
            tokio::time::advance(Duration::from_millis(50)).await;
            for h in handles {
                h.await.expect("leg completes");
            }
            max_seen.load(AtomicOrdering::SeqCst)
        }

        let bing_max = max_in_flight(&governor, SearchProvider::Bing, 3).await;
        let brave_max = max_in_flight(&governor, SearchProvider::Brave, 3).await;
        assert_eq!(
            bing_max, 2,
            "Bing's #73 default concurrency is 2: 2 of the 3 legs must overlap"
        );
        assert_eq!(
            brave_max, 1,
            "Brave's #73 default concurrency is 1: legs must never overlap"
        );
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

    #[tokio::test]
    async fn a_last_request_from_days_ago_imposes_no_wait_on_a_fresh_process() {
        // Regression: `wall_to_mono` used to clamp every past timestamp
        // (the normal case for `last_request_ms`) to "construction time",
        // making a fresh process wait almost the full gap on its very
        // first request regardless of how long ago the real last request
        // was. A persisted request from days ago must impose zero wait.
        let dir = std::env::temp_dir().join(format!("war-governor-oldreq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let days_ago_ms = now_wall_ms() - Duration::from_secs(3 * 24 * 3600).as_millis() as i64;
        let state = PersistedState {
            engines: [(
                "duckduckgo".to_string(),
                PersistedEngine {
                    suspended_until_ms: None,
                    suspend_reason: None,
                    last_request_ms: Some(days_ago_ms),
                },
            )]
            .into_iter()
            .collect(),
        };
        std::fs::write(
            dir.join(ENGINES_FILE),
            serde_json::to_vec(&state).expect("serialize"),
        )
        .expect("write engines.json");
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_MIN_MS", "2000"),
            ("SEARCH_PACE_MAX_MS", "2000"),
        ]);
        let governor = Governor::new(Some(dir.clone()));
        let start = std::time::Instant::now();
        {
            let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a 3-day-old last request must not impose the 2s gap: waited {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn over_budget_trips_after_the_configured_cap() {
        let _guard = EnvGuard::set(&[
            ("SEARCH_MAX_REQUESTS_PER_ENGINE", "2"),
            ("SEARCH_PACE_MIN_MS", "0"),
            ("SEARCH_PACE_MAX_MS", "0"),
        ]);
        let governor = Governor::new(None);
        assert!(!governor.over_budget(SearchProvider::DuckDuckGo));
        {
            let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }
        assert!(
            !governor.over_budget(SearchProvider::DuckDuckGo),
            "1 of 2 requests used"
        );
        {
            let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }
        assert!(
            governor.over_budget(SearchProvider::DuckDuckGo),
            "2 of 2 requests used, cap reached"
        );
        assert!(
            !governor.over_budget(SearchProvider::Startpage),
            "the cap is per engine, Startpage is untouched"
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
