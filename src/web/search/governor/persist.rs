//! `engines.json` persistence, its cross-process lock, and the
//! wall/monotonic clock bridge (#62, #103).
//!
//! Suspension and pacing compare against `tokio::time::Instant`; wall clock
//! is only used to carry a timestamp across process boundaries.
//!
//! #103: several processes (parallel `research` runs) share one cache
//! root. Every read-merge-write of `engines.json` happens under an
//! exclusive advisory lock on the sibling `engines.lock` file, taken with
//! `fs4` (pinned `=1.1.0`): `flock`/`LockFileEx` on every platform without
//! `unsafe` or per-OS code here; std's `File::lock` needs Rust 1.89, above
//! this crate's MSRV.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::EngineView;
use crate::web::cache_dir::write_atomic;

/// File name under the cache root (shared contract with `QueryCache`'s
/// `search/` cache entries, which live in a sibling directory).
pub(super) const ENGINES_FILE: &str = "engines.json";

/// Advisory lock file guarding every read-merge-write of [`ENGINES_FILE`]
/// (#103). Never holds data; it only exists to be locked.
pub(super) const LOCK_FILE: &str = "engines.lock";

/// On-disk shape of `engines.json`. Corrupt JSON (or a missing file) loads
/// as `Default` -- an empty map -- never an error (#62 acceptance).
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct PersistedState {
    #[serde(default)]
    pub(super) engines: HashMap<String, PersistedEngine>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct PersistedEngine {
    #[serde(default)]
    pub(super) suspended_until_ms: Option<i64>,
    /// When the suspension was recorded (#103): lets a process that saw a
    /// later success on this engine drop an older suspension while
    /// merging, and keep a newer one.
    #[serde(default)]
    pub(super) suspended_at_ms: Option<i64>,
    #[serde(default)]
    pub(super) suspend_reason: Option<String>,
    #[serde(default)]
    pub(super) last_request_ms: Option<i64>,
    /// Latest successful leg on this engine, from any process (#103): a
    /// suspension recorded at or before it is dropped while merging.
    #[serde(default)]
    pub(super) last_success_ms: Option<i64>,
}

pub(super) fn load_persisted(cache_root: &Path) -> PersistedState {
    let path = cache_root.join(ENGINES_FILE);
    let Ok(bytes) = std::fs::read(&path) else {
        return PersistedState::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Write `state` atomically. Best effort: a failure never fails a search.
pub(super) fn write_persisted(cache_root: &Path, state: &PersistedState) {
    if let Ok(bytes) = serde_json::to_vec_pretty(state) {
        let _ = write_atomic(&cache_root.join(ENGINES_FILE), &bytes);
    }
}

/// Held exclusive lock on `<cache_root>/engines.lock`; released on drop
/// (closing the file releases the advisory lock). `None` inside when the
/// lock file cannot be created or locked (read-only cache root): the
/// caller then proceeds unlocked, best effort, like every other cache
/// write.
pub(super) struct StateLock {
    _file: Option<File>,
}

/// Block until this process holds the exclusive lock on
/// `<cache_root>/engines.lock`. The critical sections it guards are one
/// small file read plus one small atomic write, never a network request.
pub(super) fn lock_state(cache_root: &Path) -> StateLock {
    let file = std::fs::create_dir_all(cache_root)
        .ok()
        .and_then(|()| {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(cache_root.join(LOCK_FILE))
                .ok()
        })
        .filter(|file| fs4::FileExt::lock(file).is_ok());
    StateLock { _file: file }
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

/// One engine's on-disk entry as a monotonic-clock [`EngineView`]
/// (`request_count` is per run and never persisted, so it starts at 0).
/// `suspended_until_ms` is clamped to `cap_wall_ms` *before* the clock
/// conversion (#103): a far-future timestamp would otherwise overflow
/// `Instant` and silently read as "no suspension".
pub(super) fn view_from_disk(
    entry: &PersistedEngine,
    cap_wall_ms: i64,
    base_wall_ms: i64,
    base_mono: Instant,
) -> EngineView {
    let to_mono = |ms: Option<i64>| ms.and_then(|ms| wall_to_mono(ms, base_wall_ms, base_mono));
    EngineView {
        suspended_until: to_mono(entry.suspended_until_ms.map(|ms| ms.min(cap_wall_ms))),
        suspended_at: to_mono(entry.suspended_at_ms),
        suspend_reason: entry.suspend_reason.clone().unwrap_or_default(),
        last_request: to_mono(entry.last_request_ms),
        last_success: to_mono(entry.last_success_ms),
        request_count: 0,
    }
}

/// Inverse of [`view_from_disk`].
pub(super) fn disk_from_view(
    view: &EngineView,
    base_wall_ms: i64,
    base_mono: Instant,
) -> PersistedEngine {
    let to_wall = |at: Option<Instant>| at.map(|at| mono_to_wall(at, base_wall_ms, base_mono));
    PersistedEngine {
        suspended_until_ms: to_wall(view.suspended_until),
        suspended_at_ms: to_wall(view.suspended_at),
        suspend_reason: (!view.suspend_reason.is_empty()).then(|| view.suspend_reason.clone()),
        last_request_ms: to_wall(view.last_request),
        last_success_ms: to_wall(view.last_success),
    }
}

/// Merge one engine's state read from disk (`disk`, already converted to
/// this process's monotonic clock) into this process's `view` (#103).
/// `last_request` and `last_success` take the later of the two. Each
/// side's suspension survives only if it is still active at `now` and was
/// recorded no earlier than the merged `last_success` (a suspension with
/// no recorded time loses to any success); of the survivors the later
/// expiry wins, so a shorter suspension never shortens a longer one. The
/// winner is clamped to `cap` (`now + max_suspension()`), and stays
/// clamped once written back, so a far-future timestamp converges.
pub(super) fn merge_view(view: &mut EngineView, disk: EngineView, now: Instant, cap: Instant) {
    view.last_request = view.last_request.max(disk.last_request);
    view.last_success = view.last_success.max(disk.last_success);
    let last_success = view.last_success;
    let alive = |until: Option<Instant>, at: Option<Instant>| {
        let until = until.filter(|until| *until > now)?;
        match (at, last_success) {
            (_, None) => Some(until),
            (Some(at), Some(success)) if at >= success => Some(until),
            _ => None,
        }
    };
    let local = alive(view.suspended_until, view.suspended_at).map(|until| {
        (
            until,
            view.suspended_at,
            std::mem::take(&mut view.suspend_reason),
        )
    });
    let remote = alive(disk.suspended_until, disk.suspended_at)
        .map(|until| (until, disk.suspended_at, disk.suspend_reason));
    let winner = match (local, remote) {
        (Some(local), Some(remote)) => Some(if remote.0 > local.0 { remote } else { local }),
        (local, remote) => local.or(remote),
    };
    match winner {
        Some((until, at, reason)) => {
            view.suspended_until = Some(until.min(cap));
            view.suspended_at = at;
            view.suspend_reason = reason;
        }
        None => {
            view.suspended_until = None;
            view.suspended_at = None;
            view.suspend_reason.clear();
        }
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
                    last_request_ms: Some(days_ago_ms),
                    ..PersistedEngine::default()
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

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("war-governor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn challenge(provider: SearchProvider) -> SearchProviderError {
        SearchProviderError::Challenge {
            provider,
            detail: "walled".to_string(),
        }
    }

    #[tokio::test]
    async fn two_governors_on_one_root_keep_each_others_suspensions() {
        // #103: each governor used to write its own whole map, so the
        // last writer dropped the other's suspension.
        let dir = temp_root("two-writers");
        let first = Governor::new(Some(dir.clone()));
        let second = Governor::new(Some(dir.clone()));
        // Both load (empty) state before either records anything.
        assert!(first.suspended_remaining(SearchProvider::Brave).is_none());
        assert!(second.suspended_remaining(SearchProvider::Bing).is_none());
        first.record_failure(SearchProvider::Brave, &challenge(SearchProvider::Brave));
        second.record_failure(SearchProvider::Bing, &challenge(SearchProvider::Bing));
        // `second` also records an unrelated success: it must not erase
        // `first`'s Brave suspension, which `second` never observed.
        second.record_success(SearchProvider::Yahoo);

        let on_disk = load_persisted(&dir);
        for id in ["brave", "bing"] {
            assert_eq!(
                on_disk.engines[id].suspend_reason.as_deref(),
                Some("challenge"),
                "{id} suspension lost: {on_disk:?}"
            );
        }
        let fresh = Governor::new(Some(dir.clone()));
        assert!(fresh.suspended_remaining(SearchProvider::Brave).is_some());
        assert!(fresh.suspended_remaining(SearchProvider::Bing).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_success_recorded_after_a_suspension_clears_it_for_other_processes() {
        let dir = temp_root("success-wins");
        let first = Governor::new(Some(dir.clone()));
        first.record_failure(SearchProvider::Brave, &challenge(SearchProvider::Brave));
        let second = Governor::new(Some(dir.clone()));
        assert!(second.suspended_remaining(SearchProvider::Brave).is_some());
        // Persisted times have millisecond resolution: keep the success
        // strictly after the suspension on disk.
        std::thread::sleep(std::time::Duration::from_millis(5));
        second.record_success(SearchProvider::Brave);
        // `first` still holds the suspension in memory; its next locked
        // refresh merges the later success and drops it.
        first.refresh();
        assert!(first.suspended_remaining(SearchProvider::Brave).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_week_long_retry_after_is_clamped_to_the_largest_suspension() {
        let _guard = EnvGuard::set(&[]);
        let dir = temp_root("retry-after-clamp");
        let governor = Governor::new(Some(dir.clone()));
        governor.record_failure(
            SearchProvider::DuckDuckGo,
            &SearchProviderError::Upstream {
                provider: SearchProvider::DuckDuckGo,
                status: Some(429),
                retry_after_secs: Some(604_800),
                detail: "slow down".to_string(),
            },
        );
        let (remaining, reason) = governor
            .suspended_remaining(SearchProvider::DuckDuckGo)
            .expect("a Retry-After still suspends");
        assert_eq!(reason, "retry_after");
        assert!(remaining <= 3600, "not clamped: {remaining}s");
        let until = load_persisted(&dir).engines["duckduckgo"]
            .suspended_until_ms
            .expect("persisted");
        assert!(until <= now_wall_ms() + 3_600_000 + 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_far_future_suspension_on_disk_is_clamped_on_load_and_kept() {
        let _guard = EnvGuard::set(&[]);
        let dir = temp_root("far-future");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = PersistedState {
            engines: [(
                "brave".to_string(),
                PersistedEngine {
                    suspended_until_ms: Some(i64::MAX / 2),
                    suspend_reason: Some("challenge".to_string()),
                    ..PersistedEngine::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        write_persisted(&dir, &state);
        let governor = Governor::new(Some(dir.clone()));
        let (remaining, reason) = governor
            .suspended_remaining(SearchProvider::Brave)
            .expect("a far-future suspension must still suspend, not overflow to none");
        assert_eq!(reason, "challenge");
        assert!(remaining <= 3600, "not clamped: {remaining}s");
        let until = load_persisted(&dir).engines["brave"]
            .suspended_until_ms
            .expect("still persisted");
        assert!(
            until <= now_wall_ms() + 3_600_000 + 1000,
            "clamped value written back: {until}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
