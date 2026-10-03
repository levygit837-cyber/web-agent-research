//! Test-only env-var guard for every `SEARCH_*` key this module's
//! env-reading functions consult, shared across `governor::tests` and
//! `fanout::tests` (both mutate these and run in the same test binary;
//! same pattern as `cache::test_support::EnvGuard`).
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
        crate::test_support::clear_env_keys(&GOVERNOR_KEYS);
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
        crate::test_support::clear_env_keys(&GOVERNOR_KEYS);
    }
}
