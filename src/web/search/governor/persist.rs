//! `engines.json` persistence and the wall/monotonic clock bridge (#62).
//!
//! Suspension and pacing compare against `tokio::time::Instant`; wall clock
//! is only used to carry a timestamp across process boundaries.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

/// File name under the cache root (shared contract with `QueryCache`'s
/// `search/` cache entries, which live in a sibling directory).
pub(super) const ENGINES_FILE: &str = "engines.json";

/// On-disk shape of `engines.json`. Corrupt JSON (or a missing file) loads
/// as `Default` -- an empty map -- never an error (#62 acceptance).
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct PersistedState {
    #[serde(default)]
    pub(super) engines: HashMap<String, PersistedEngine>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct PersistedEngine {
    #[serde(default)]
    pub(super) suspended_until_ms: Option<i64>,
    #[serde(default)]
    pub(super) suspend_reason: Option<String>,
    #[serde(default)]
    pub(super) last_request_ms: Option<i64>,
}

pub(super) fn load_persisted(cache_root: &Path) -> PersistedState {
    let path = cache_root.join(ENGINES_FILE);
    let Ok(bytes) = std::fs::read(&path) else {
        return PersistedState::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub(super) fn now_wall_ms() -> i64 {
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
pub(super) fn wall_to_mono(ms: i64, base_wall_ms: i64, base_mono: Instant) -> Option<Instant> {
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
pub(super) fn mono_to_wall(instant: Instant, base_wall_ms: i64, base_mono: Instant) -> i64 {
    if instant >= base_mono {
        let delta = instant.saturating_duration_since(base_mono).as_millis() as i64;
        base_wall_ms.saturating_add(delta)
    } else {
        let delta = base_mono.saturating_duration_since(instant).as_millis() as i64;
        base_wall_ms.saturating_sub(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::governor::test_support::EnvGuard;
    use crate::web::search::governor::{Governor, PaceOutcome};
    use crate::web::search::types::{SearchProvider, SearchProviderError};

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
}
