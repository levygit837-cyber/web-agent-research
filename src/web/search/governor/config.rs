//! Pure, env-only resolvers for the governor: engine allowlist (#64),
//! suspension durations (#62), pacing gaps (#63), pagination depth,
//! concurrency, and per-run/per-call request caps (#73).
//!
//! Each resolver reads `SEARCH_*` env vars; which ones are re-read per call
//! and which once at [`Governor::new`](super::Governor::new) is documented
//! on each function and in the parent module doc.

use std::collections::HashMap;
use std::time::Duration;

use rand::Rng;

use super::ALL_PROVIDERS;
use crate::web::search::types::{SearchProvider, SearchProviderError};

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
/// `startpage`/`sp`, `brave`, `yahoo`, `bing`, `cratesio`/`crates.io`
/// tokens; unknown tokens ignored; duplicates collapsed; unset or
/// resolving empty defaults to `duckduckgo,brave,yahoo,bing,cratesio` --
/// #64/#66: Startpage stays opt-in only (it serves an Anubis proof-of-work
/// challenge to bot-detected clients); #109: the crates.io vertical is on
/// by default and still only joins crate-shaped Queries.
/// Pure and env-only, so it is unit-tested directly without any HTTP.
pub(super) fn enabled_providers() -> Vec<SearchProvider> {
    let raw = std::env::var("SEARCH_ENGINES")
        .unwrap_or_else(|_| "duckduckgo,brave,yahoo,bing,cratesio".to_string());
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
            "cratesio" | "crates.io" => Some(SearchProvider::CratesIo),
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
        SearchProvider::CratesIo,
    ];
    providers.sort_by_key(|p| p.priority());
    providers
}

/// One suspension decision: how long, and the stable reason string
/// persisted alongside it.
pub(super) struct Suspension {
    pub(super) duration: Duration,
    pub(super) reason: &'static str,
}

/// Map a settled leg error to a suspension decision, or `None` for "never
/// suspend on this kind" (#62). Defaults inspired by SearXNG
/// `search.suspended_times` (idea only, see module doc), each independently
/// overridable via env:
///
/// - `Challenge` (bot wall) -> `SEARCH_SUSPEND_CHALLENGE_SECS`, default
///   3600 s: the strongest signal this network is walled; long enough that
///   a run a few minutes later does not re-provoke the same challenge.
/// - `Upstream` with a real `Retry-After` -> that value wins over the
///   status default (#62), clamped to [`max_suspension`] (#103: a hostile
///   or buggy `Retry-After: 604800` must not park an engine for a week).
/// - `Upstream` HTTP 403 -> `SEARCH_SUSPEND_403_SECS`, default 180 s: an
///   access-denial that is usually IP-reputation based and often lifts
///   within minutes.
/// - `Upstream` HTTP 429 -> `SEARCH_SUSPEND_429_SECS`, default 180 s: a
///   rate limit, same reasoning as 403.
/// - `Upstream` HTTP 5xx -> `SEARCH_SUSPEND_UPSTREAM_SECS`, default 30 s:
///   most likely a transient server-side error, not a bot wall; a short
///   backoff avoids hammering during an outage without punishing the
///   engine once it recovers.
/// - `Upstream` with no status (#103: DNS, connect, TLS or body-read
///   failure -- no response was received) or any other status (a 2xx
///   with unrecognized markup, a 404) -> never suspends: the engine never
///   answered with an error of its own. The leg retries a transport
///   failure once instead (`fanout::leg`).
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
pub(super) fn suspension_for(err: &SearchProviderError) -> Option<Suspension> {
    match err {
        SearchProviderError::Challenge { .. } => Some(Suspension {
            duration: Duration::from_secs(challenge_secs()),
            reason: "challenge",
        }),
        SearchProviderError::Upstream {
            status,
            retry_after_secs,
            ..
        } => {
            if let Some(secs) = retry_after_secs {
                return Some(Suspension {
                    duration: Duration::from_secs(*secs).min(max_suspension()),
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
                Some(500..=599) => {
                    let secs = upstream_secs();
                    (secs > 0).then(|| Suspension {
                        duration: Duration::from_secs(secs),
                        reason: "upstream",
                    })
                }
                _ => None,
            }
        }
        SearchProviderError::Timeout { .. } => {
            let secs = timeout_secs();
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

fn challenge_secs() -> u64 {
    env_u64("SEARCH_SUSPEND_CHALLENGE_SECS", 3600)
}

fn upstream_secs() -> u64 {
    env_u64("SEARCH_SUSPEND_UPSTREAM_SECS", 30)
}

fn timeout_secs() -> u64 {
    env_u64("SEARCH_SUSPEND_TIMEOUT_SECS", 0)
}

/// The longest suspension any configured kind can record (#103): the max
/// of every `SEARCH_SUSPEND_*_SECS` value (default: the 3600 s challenge
/// suspension). A `Retry-After` and every suspension loaded from or
/// recorded to `engines.json` is clamped to `now + max_suspension()`, so a
/// far-future timestamp (a hostile header, clock skew, a hand-edited file)
/// converges to this bound instead of parking an engine indefinitely.
/// Re-read from env on every call, like [`suspension_for`].
pub(super) fn max_suspension() -> Duration {
    let secs = [
        challenge_secs(),
        env_u64("SEARCH_SUSPEND_403_SECS", 180),
        env_u64("SEARCH_SUSPEND_429_SECS", 180),
        upstream_secs(),
        timeout_secs(),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    Duration::from_secs(secs)
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
/// crates.io (#109) is a documented policy, not a measurement: at most 1
/// API request per second, so its gap never goes below 1000 ms (the
/// process-wide `web::crates_io::API_GATE` enforces the same floor for
/// the API calls the crate-page fetch makes).
fn engine_gap_default_ms(provider: SearchProvider) -> (u64, u64) {
    match provider {
        SearchProvider::Bing | SearchProvider::Yahoo => (1000, 2000),
        SearchProvider::CratesIo => (1000, 1500),
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
pub(super) fn resolve_gap(provider: SearchProvider) -> (Duration, Duration) {
    let engine = provider.id().to_uppercase();
    let (hardcoded_min, hardcoded_max) = engine_gap_default_ms(provider);
    let global_min = env_u64("SEARCH_PACE_MIN_MS", hardcoded_min);
    let global_max = env_u64("SEARCH_PACE_MAX_MS", hardcoded_max);
    let min = env_u64(&format!("SEARCH_PACE_{engine}_MIN_MS"), global_min);
    let max = env_u64(&format!("SEARCH_PACE_{engine}_MAX_MS"), global_max).max(min);
    (Duration::from_millis(min), Duration::from_millis(max))
}

pub(super) fn pick_gap(min: Duration, max: Duration) -> Duration {
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
/// round). crates.io asks page 1 only (#109).
fn engine_max_pages_default(provider: SearchProvider) -> u64 {
    match provider {
        SearchProvider::Yahoo => 3,
        SearchProvider::DuckDuckGo => 5,
        SearchProvider::Bing
        | SearchProvider::Brave
        | SearchProvider::Startpage
        | SearchProvider::CratesIo => 1,
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
/// #73's scope; unmeasured this round). crates.io is 1: its policy is one
/// request at a time, 1 s apart (#109).
fn engine_concurrency_default(provider: SearchProvider) -> u64 {
    match provider {
        SearchProvider::Bing | SearchProvider::Yahoo => 2,
        SearchProvider::Brave
        | SearchProvider::DuckDuckGo
        | SearchProvider::Startpage
        | SearchProvider::CratesIo => 1,
    }
}

/// Resolve one engine's concurrency limit: `SEARCH_CONCURRENCY_<ENGINE>`
/// overrides the engine; `SEARCH_CONCURRENCY` overrides every engine
/// uniformly when set; else [`engine_concurrency_default`]. Read once, at
/// [`Governor::new`](super::Governor::new) construction (sizes each engine's `Semaphore`), not
/// re-read per call -- unlike the gap/suspension resolvers, a live
/// `Governor`'s concurrency limit cannot change after its queues are
/// built without adding/removing `Semaphore` permits at runtime, which no
/// caller needs.
pub(super) fn resolve_concurrency(provider: SearchProvider) -> usize {
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
/// (out of #73's scope; unmeasured this round). crates.io (#109) gets 30:
/// one request per crate-shaped Query, enough for a full run (the agent
/// loop's turn budget times a few crate Queries per call) while keeping
/// one run from turning into a crawl of a volunteer-run registry.
fn engine_cap_default(provider: SearchProvider) -> u32 {
    match provider {
        SearchProvider::Bing | SearchProvider::Yahoo | SearchProvider::Startpage => 200,
        SearchProvider::CratesIo => 30,
        SearchProvider::Brave | SearchProvider::DuckDuckGo => 10,
    }
}

/// Resolve every provider's per-run request cap once, at construction
/// (the module doc: unlike suspension/gap, both request caps are read
/// once, not re-read per call). `SEARCH_MAX_REQUESTS_PER_ENGINE_<ENGINE>`
/// overrides the engine; `SEARCH_MAX_REQUESTS_PER_ENGINE` (global,
/// pre-#73) overrides every engine uniformly when set; else
/// [`engine_cap_default`].
pub(super) fn resolve_caps_per_engine() -> HashMap<SearchProvider, u32> {
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
pub(super) fn resolve_max_queries_per_engine() -> HashMap<SearchProvider, usize> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::governor::test_support::EnvGuard;

    #[test]
    fn enabled_providers_defaults_to_ddg_brave_yahoo_bing_cratesio() {
        let _guard = EnvGuard::set(&[]);
        std::env::remove_var("SEARCH_ENGINES");
        let providers = enabled_providers();
        assert!(!providers.contains(&SearchProvider::Startpage));
        // Priority order, not input/declaration order.
        assert_eq!(
            providers,
            vec![
                SearchProvider::Brave,
                SearchProvider::Yahoo,
                SearchProvider::DuckDuckGo,
                SearchProvider::Bing,
                SearchProvider::CratesIo,
            ]
        );
    }

    #[test]
    fn enabled_providers_accepts_both_cratesio_tokens() {
        for token in ["cratesio", "Crates.IO"] {
            let _guard = EnvGuard::set(&[("SEARCH_ENGINES", token)]);
            assert_eq!(enabled_providers(), vec![SearchProvider::CratesIo]);
        }
        let _guard = EnvGuard::set(&[("SEARCH_ENGINES", "bing")]);
        assert_eq!(enabled_providers(), vec![SearchProvider::Bing]);
    }

    #[test]
    fn cratesio_defaults_honor_the_one_request_per_second_policy() {
        let _guard = EnvGuard::set(&[]);
        let (min, max) = resolve_gap(SearchProvider::CratesIo);
        assert!(min >= Duration::from_millis(1000), "{min:?}");
        assert!(max >= min);
        assert_eq!(resolve_concurrency(SearchProvider::CratesIo), 1);
        assert_eq!(resolve_max_pages(SearchProvider::CratesIo), 1);
        assert_eq!(resolve_caps_per_engine()[&SearchProvider::CratesIo], 30);
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
}
