//! Per-engine-leg disk cache for search results (ADR-0002: files, no DB).
//!
//! One entry per (provider, normalized query, recency, page) under
//! `<cache_root>/search/`, keyed by a content hash so filenames never leak
//! the raw query. Cached per leg, not per merged fan-out output: a cached
//! leg for one engine and a live leg for another still merge normally.
//! Every provider slots in via `provider.id()` alone (#66: Brave/Yahoo/Bing
//! joined DuckDuckGo/Startpage without touching this file).
//!
//! `lookup`/`store` take `cache_root: Option<&Path>` directly and no-op on
//! `None` (the hermetic seam: `Searcher::with_bases` and every test
//! constructor pass `None`, never resolving `cache_dir::cache_root()`) and
//! on `SEARCH_CACHE=off`. `store` takes rows, not a `Result`: a Challenge or
//! any other leg error has no code path into this cache by construction --
//! the caller (`fanout::leg::run`) only calls `store` on a settled `Ok`
//! leg, and `store` itself skips zero rows (#104).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::types::{Recency, SearchProvider, SearchResult};
use crate::web::cache_dir::{write_atomic, TEMP_EXTENSION_PREFIX};

/// Default entry lifetime: 24 h (issue #67: "none longer").
const DEFAULT_TTL_SECS: u64 = 24 * 3_600;
/// Entry lifetime when the Query carries `recency: day`: results for a
/// same-day window go stale faster than an undated Query.
const DAY_RECENCY_TTL_SECS: u64 = 3_600;
/// Default size cap for `<cache_root>/search/`, oldest-first eviction.
const DEFAULT_MAX_BYTES: u64 = 20 * 1_024 * 1_024;

/// On-disk shape: parsed rows + fetched-at (issue #67 value spec verbatim).
/// TTL is not stored: it is a function of `recency`, evaluated at lookup
/// time, so a `SEARCH_CACHE_TTL_SECS` env change takes effect on existing
/// entries without a rewrite.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    fetched_at_secs: u64,
    results: Vec<SearchResult>,
}

/// `SEARCH_CACHE=off` disables both `lookup` (always miss) and `store`
/// (always no-op), independent of `cache_root`.
fn cache_disabled() -> bool {
    std::env::var("SEARCH_CACHE")
        .map(|v| v.trim() == "off")
        .unwrap_or(false)
}

/// `SEARCH_CACHE_TTL_SECS` overrides the TTL for every recency uniformly;
/// unset falls back to the day-vs-rest split. An invalid value (not a
/// `u64`) is ignored rather than erroring: `Searcher::new()` is not
/// fallible, so a malformed override degrades to the default instead of
/// panicking or blocking construction.
fn ttl_secs(recency: Option<Recency>) -> u64 {
    if let Some(secs) = std::env::var("SEARCH_CACHE_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return secs;
    }
    if recency == Some(Recency::Day) {
        DAY_RECENCY_TTL_SECS
    } else {
        DEFAULT_TTL_SECS
    }
}

/// `SEARCH_CACHE_MAX_BYTES` overrides the eviction cap; invalid falls back
/// to the default for the same reason as `ttl_secs`.
fn max_bytes() -> u64 {
    std::env::var("SEARCH_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// Normalize a raw Query for the cache key: trim, collapse internal
/// whitespace runs to one space, lowercase. `split_whitespace` already
/// trims and collapses, so this is one pass.
fn normalize_query(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// FNV-1a 64-bit: a fixed, dependency-free hash so the cache filename does
/// not depend on `std::collections::hash_map::DefaultHasher`'s unspecified,
/// version-unstable algorithm (a std upgrade would otherwise silently
/// orphan every existing entry). A cache-key collision only costs a wrong
/// hit that a real request would still correct on the next TTL cycle, so a
/// non-cryptographic hash is the right tool here.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Cache key: normalized query + recency + provider + page (issue #67).
/// `\u{1}` (a control char no query text contains) separates fields so
/// `"a\u{1}b"` and `"a b"` never collide.
fn cache_key(
    provider: SearchProvider,
    query: &str,
    recency: Option<Recency>,
    page: usize,
) -> String {
    let normalized = normalize_query(query);
    let recency_tag = recency.map(Recency::param).unwrap_or("none");
    format!(
        "{}\u{1}{normalized}\u{1}{recency_tag}\u{1}{page}",
        provider.id()
    )
}

fn entry_path(search_dir: &Path, key: &str) -> PathBuf {
    search_dir.join(format!("{:016x}.json", fnv1a64(key.as_bytes())))
}

/// Look up a cached leg. `None` on: cache disabled, no `cache_root`
/// (hermetic), no entry on disk, a corrupt/unparseable entry, or an entry
/// older than `ttl_secs(recency)`. Never touches the network or panics.
pub(crate) fn lookup(
    cache_root: Option<&Path>,
    provider: SearchProvider,
    query: &str,
    recency: Option<Recency>,
    page: usize,
) -> Option<Vec<SearchResult>> {
    if cache_disabled() {
        return None;
    }
    let search_dir = cache_root?.join("search");
    let path = entry_path(&search_dir, &cache_key(provider, query, recency, page));
    let bytes = std::fs::read(path).ok()?;
    let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age_secs = now_secs.saturating_sub(entry.fetched_at_secs);
    if age_secs > ttl_secs(recency) {
        return None;
    }
    Some(entry.results)
}

/// Store a settled leg's rows. No-op on cache disabled or no `cache_root`.
/// Callers pass rows only, never a `Result`: a Challenge, transport error
/// or unrecognized-markup leg has no way to reach this function. The
/// fan-out never calls it with zero rows either (#104): an empty answer
/// is cheap to ask again and must not shadow a later real one for 24 h.
pub(crate) fn store(
    cache_root: Option<&Path>,
    provider: SearchProvider,
    query: &str,
    recency: Option<Recency>,
    page: usize,
    results: &[SearchResult],
) {
    if cache_disabled() || results.is_empty() {
        return;
    }
    let Some(root) = cache_root else {
        return;
    };
    let search_dir = root.join("search");
    let path = entry_path(&search_dir, &cache_key(provider, query, recency, page));
    let fetched_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = CacheEntry {
        fetched_at_secs,
        results: results.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec(&entry) else {
        return;
    };
    if write_atomic(&path, &bytes).is_err() {
        return;
    }
    enforce_cap(&search_dir, max_bytes());
}

/// Age past which a `write_atomic` temp file (`*.tmp-<pid>-<n>`) is an
/// orphan of a killed process, not an in-flight write (#103). A real
/// write lasts milliseconds.
const STALE_TEMP_SECS: u64 = 600;

/// Oldest-first eviction by `fetched_at_secs` (not filesystem mtime: some
/// filesystems have coarse mtime resolution, and `fetched_at_secs` is the
/// field that actually defines "oldest" for this cache). A corrupt entry
/// sorts as `fetched_at_secs = 0`, i.e. oldest, so it is evicted before any
/// valid entry rather than lingering. Also removes `write_atomic` temp
/// files older than [`STALE_TEMP_SECS`] (#103). Best effort: an unreadable
/// directory or a `remove_file` failure is skipped, never panics.
fn enforce_cap(search_dir: &Path, cap_bytes: u64) {
    let Ok(read_dir) = std::fs::read_dir(search_dir) else {
        return;
    };
    // Pass 1: sizes only, from directory metadata -- no read/parse of any
    // entry body. `store` calls `enforce_cap` on every successful leg, so
    // the common case (well under the cap) must stay a cheap `stat` walk,
    // not a full read+deserialize of every cached entry.
    struct Candidate {
        path: PathBuf,
        len: u64,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut total: u64 = 0;
    for dir_entry in read_dir.flatten() {
        let path = dir_entry.path();
        let Ok(metadata) = dir_entry.metadata() else {
            continue;
        };
        let extension = path.extension().and_then(|ext| ext.to_str());
        if extension.is_some_and(|ext| ext.starts_with(TEMP_EXTENSION_PREFIX)) {
            // A fresh temp is an in-flight write: leave it alone.
            let stale = metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age.as_secs() > STALE_TEMP_SECS);
            if stale {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        // Anything else that is not one of our entries is skipped.
        if extension != Some("json") {
            continue;
        }
        let len = metadata.len();
        total += len;
        candidates.push(Candidate { path, len });
    }
    if total <= cap_bytes {
        return;
    }
    // Pass 2: only reached over the cap -- now read+parse each entry's
    // `fetched_at_secs` to sort oldest-first. A corrupt entry sorts as
    // `fetched_at_secs = 0`, i.e. oldest, so it is evicted before any
    // valid entry rather than lingering. Best effort: an unreadable
    // directory, an unparseable entry, or a `remove_file` failure is
    // skipped, never panics.
    struct Item {
        path: PathBuf,
        len: u64,
        fetched_at_secs: u64,
    }
    let mut items: Vec<Item> = candidates
        .into_iter()
        .map(|candidate| {
            let fetched_at_secs = std::fs::read(&candidate.path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<CacheEntry>(&bytes).ok())
                .map(|entry| entry.fetched_at_secs)
                .unwrap_or(0);
            Item {
                path: candidate.path,
                len: candidate.len,
                fetched_at_secs,
            }
        })
        .collect();
    items.sort_by_key(|item| item.fetched_at_secs);
    for item in items {
        if total <= cap_bytes {
            break;
        }
        if std::fs::remove_file(&item.path).is_ok() {
            total = total.saturating_sub(item.len);
        }
    }
}

/// Test-only env-var guard shared across `cache::tests` and
/// `fanout::tests`: both mutate `SEARCH_CACHE*` and run in the same test
/// binary, so a test in either module that does not hold this lock could
/// observe another test's override mid-run (full-suite-safe requirement).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    const CACHE_KEYS: [&str; 3] = [
        "SEARCH_CACHE",
        "SEARCH_CACHE_TTL_SECS",
        "SEARCH_CACHE_MAX_BYTES",
    ];

    /// Serializes tests that mutate `SEARCH_CACHE*` env vars and clears
    /// them on drop, so one test's override never leaks into the next
    /// (same pattern as `llm::config::tests::EnvGuard`, delegated to
    /// `crate::test_support::clear_env_keys`).
    pub(crate) struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        pub(crate) fn lock() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            crate::test_support::clear_env_keys(&CACHE_KEYS);
            Self { _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            crate::test_support::clear_env_keys(&CACHE_KEYS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::EnvGuard;
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "war-cache-test-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp cache root");
        dir
    }

    fn row(url: &str, query: &str) -> SearchResult {
        SearchResult {
            title: "t".to_string(),
            url: url.to_string(),
            snippet: "s".to_string(),
            provider: SearchProvider::DuckDuckGo,
            query: query.to_string(),
            rank: 0,
            published_date: None,
        }
    }

    #[test]
    fn none_cache_root_is_hermetic_no_op() {
        let _guard = EnvGuard::lock();
        // The `with_bases`/test seam: no root at all. Neither call must
        // touch disk, and lookup must always miss.
        store(
            None,
            SearchProvider::DuckDuckGo,
            "q",
            None,
            0,
            &[row("https://x/", "q")],
        );
        assert_eq!(lookup(None, SearchProvider::DuckDuckGo, "q", None, 0), None);
    }

    #[test]
    fn search_cache_off_disables_lookup_and_store() {
        let _guard = EnvGuard::lock();
        let root = temp_dir("off");
        std::env::set_var("SEARCH_CACHE", "off");
        store(
            Some(&root),
            SearchProvider::DuckDuckGo,
            "q",
            None,
            0,
            &[row("https://x/", "q")],
        );
        // `store` under `off` must be a real no-op, not just a lookup that
        // happens to also miss: the `search/` directory must not even
        // exist yet.
        assert!(
            !root.join("search").exists(),
            "store must not write anything while SEARCH_CACHE=off"
        );
        // Even a manually-planted valid entry must not be served while off.
        std::env::remove_var("SEARCH_CACHE");
        store(
            Some(&root),
            SearchProvider::DuckDuckGo,
            "q",
            None,
            0,
            &[row("https://x/", "q")],
        );
        std::env::set_var("SEARCH_CACHE", "off");
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "q", None, 0),
            None
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn normalization_hits_the_same_entry() {
        let _guard = EnvGuard::lock();
        let root = temp_dir("norm");
        let rows = vec![row("https://example.com/a", "hello world")];
        store(
            Some(&root),
            SearchProvider::DuckDuckGo,
            "  Hello   World  ",
            None,
            0,
            &rows,
        );
        // Different casing/whitespace, same normalized key -> hit.
        let hit = lookup(
            Some(&root),
            SearchProvider::DuckDuckGo,
            "hello world",
            None,
            0,
        );
        assert_eq!(hit, Some(rows));
        // A different provider, recency, or page is a distinct key -> miss.
        assert_eq!(
            lookup(
                Some(&root),
                SearchProvider::Startpage,
                "hello world",
                None,
                0
            ),
            None
        );
        assert_eq!(
            lookup(
                Some(&root),
                SearchProvider::DuckDuckGo,
                "hello world",
                Some(Recency::Week),
                0
            ),
            None
        );
        assert_eq!(
            lookup(
                Some(&root),
                SearchProvider::DuckDuckGo,
                "hello world",
                None,
                1
            ),
            None
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn expired_entry_is_ignored() {
        let _guard = EnvGuard::lock();
        let root = temp_dir("expiry");
        let search_dir = root.join("search");
        let key = cache_key(SearchProvider::DuckDuckGo, "q", None, 0);
        let path = entry_path(&search_dir, &key);
        // Plant an entry stamped far enough in the past that even the
        // 24h default TTL has elapsed; a real `store()` always stamps
        // `fetched_at_secs` at now, so this simulates the passage of time
        // without a real (flaky) sleep.
        let stale = CacheEntry {
            fetched_at_secs: 1,
            results: vec![row("https://x/", "q")],
        };
        write_atomic(&path, &serde_json::to_vec(&stale).unwrap()).expect("plant stale entry");
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "q", None, 0),
            None,
            "an entry older than the TTL must not be served"
        );
        // A short override makes a *fresh* entry expire deterministically
        // too, proving the env override is honored (not just the default).
        std::env::set_var("SEARCH_CACHE_TTL_SECS", "0");
        store(
            Some(&root),
            SearchProvider::DuckDuckGo,
            "fresh",
            None,
            0,
            &[row("https://y/", "fresh")],
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "fresh", None, 0),
            None,
            "TTL=0 must expire on the very next second"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn day_recency_gets_a_shorter_default_ttl_than_undated() {
        let _guard = EnvGuard::lock();
        // No override: day-recency TTL is strictly shorter than the
        // undated default (issue #67: "day short, none longer").
        assert!(ttl_secs(Some(Recency::Day)) < ttl_secs(None));
        assert_eq!(ttl_secs(Some(Recency::Day)), DAY_RECENCY_TTL_SECS);
        assert_eq!(ttl_secs(None), DEFAULT_TTL_SECS);
        // The env override applies uniformly to every recency.
        std::env::set_var("SEARCH_CACHE_TTL_SECS", "42");
        assert_eq!(ttl_secs(Some(Recency::Day)), 42);
        assert_eq!(ttl_secs(None), 42);
        // An invalid override is ignored, not a panic or a hard error.
        std::env::set_var("SEARCH_CACHE_TTL_SECS", "not-a-number");
        assert_eq!(ttl_secs(None), DEFAULT_TTL_SECS);
    }

    #[test]
    fn store_then_lookup_roundtrips() {
        let _guard = EnvGuard::lock();
        let root = temp_dir("roundtrip");
        // Unit-level roundtrip for `store`'s own contract: it takes rows,
        // never a `Result`, so there is no branch inside `store` itself
        // that could persist an error leg -- the guarantee that a real
        // Challenge/Timeout/Upstream leg never reaches `store` at all is
        // enforced by the call site in `fanout/leg.rs`, covered by
        // `fanout::tests::cache::challenged_leg_is_never_cached`, not by this
        // function.
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "q", None, 0),
            None,
            "nothing stored yet must miss"
        );
        let rows = vec![row("https://x/", "q")];
        store(Some(&root), SearchProvider::DuckDuckGo, "q", None, 0, &rows);
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "q", None, 0),
            Some(rows)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn zero_rows_are_never_cached() {
        // #104: an empty leg (a genuine no-results page, or drift briefly
        // mistaken for one) must not answer the same Query for 24 h.
        let _guard = EnvGuard::lock();
        let root = temp_dir("zero");
        store(
            Some(&root),
            SearchProvider::DuckDuckGo,
            "no hits query",
            None,
            0,
            &[],
        );
        assert_eq!(
            lookup(
                Some(&root),
                SearchProvider::DuckDuckGo,
                "no hits query",
                None,
                0
            ),
            None
        );
        assert!(!root.join("search").exists(), "nothing written");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn eviction_removes_stale_temp_files_and_keeps_fresh_ones() {
        // #103: a temp file orphaned by a killed writer is evicted; one
        // that is still being written (fresh mtime) is left alone.
        let _guard = EnvGuard::lock();
        let root = temp_dir("stale-tmp");
        let search_dir = root.join("search");
        std::fs::create_dir_all(&search_dir).expect("mkdir");
        let stale = search_dir.join(format!("abc.{TEMP_EXTENSION_PREFIX}1-0"));
        let fresh = search_dir.join(format!("def.{TEMP_EXTENSION_PREFIX}1-1"));
        std::fs::write(&stale, b"x").expect("write stale");
        std::fs::write(&fresh, b"x").expect("write fresh");
        let old = SystemTime::now() - std::time::Duration::from_secs(STALE_TEMP_SECS + 60);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .and_then(|file| file.set_modified(old))
            .expect("age stale temp");
        enforce_cap(&search_dir, u64::MAX);
        assert!(!stale.exists(), "stale temp evicted");
        assert!(fresh.exists(), "in-flight temp kept");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn corrupt_entry_is_ignored_then_overwritten() {
        let _guard = EnvGuard::lock();
        let root = temp_dir("corrupt");
        let search_dir = root.join("search");
        let key = cache_key(SearchProvider::DuckDuckGo, "q", None, 0);
        let path = entry_path(&search_dir, &key);
        write_atomic(&path, b"{ not json").expect("plant corrupt entry");
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "q", None, 0),
            None,
            "a corrupt entry must be ignored, never panic"
        );
        let rows = vec![row("https://fresh.example/", "q")];
        store(Some(&root), SearchProvider::DuckDuckGo, "q", None, 0, &rows);
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "q", None, 0),
            Some(rows),
            "store must overwrite a corrupt entry at the same key"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn eviction_keeps_the_cap_removing_the_oldest_first() {
        let _guard = EnvGuard::lock();
        let root = temp_dir("evict");
        let search_dir = root.join("search");
        // Three entries with distinct, controlled `fetched_at_secs` planted
        // directly (bypassing `store`'s "now" stamp so ordering is exact,
        // not dependent on filesystem mtime resolution or sleeps). Each
        // holds one row with a long URL so its on-disk size is predictable
        // and a single entry cannot alone fit under the cap.
        let big_url = format!("https://example.com/{}", "x".repeat(200));
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let plant = |tag: &str, fetched_at_secs: u64| {
            let key = cache_key(SearchProvider::DuckDuckGo, tag, None, 0);
            let path = entry_path(&search_dir, &key);
            let entry = CacheEntry {
                fetched_at_secs,
                results: vec![row(&big_url, tag)],
            };
            write_atomic(&path, &serde_json::to_vec(&entry).unwrap()).expect("plant entry");
        };
        // Timestamps relative to "now" (200s/100s/0s ago), so `lookup`'s
        // TTL check never rejects them regardless of when this test runs
        // -- only `enforce_cap`'s oldest-first ordering is under test here.
        plant("oldest", now_secs - 200);
        plant("middle", now_secs - 100);
        plant("newest", now_secs);
        let one_entry_bytes = std::fs::read_dir(&search_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .metadata()
            .unwrap()
            .len();
        // Cap fits ~1.5 entries: triggers eviction, keeps the newest.
        let cap = one_entry_bytes + one_entry_bytes / 2;
        enforce_cap(&search_dir, cap);
        let remaining: u64 = std::fs::read_dir(&search_dir)
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(
            remaining <= cap,
            "eviction must bring total size under the cap"
        );
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "newest", None, 0),
            Some(vec![row(&big_url, "newest")]),
            "the newest entry must survive eviction"
        );
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "oldest", None, 0),
            None,
            "the oldest entry must be evicted first"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn eviction_runs_automatically_from_store() {
        let _guard = EnvGuard::lock();
        let root = temp_dir("evict-store");
        let big_url = format!("https://example.com/{}", "y".repeat(200));
        std::env::set_var("SEARCH_CACHE_MAX_BYTES", "1");
        store(
            Some(&root),
            SearchProvider::DuckDuckGo,
            "only",
            None,
            0,
            &[row(&big_url, "only")],
        );
        // A cap of 1 byte is smaller than any real entry: `store` itself
        // must evict what it just wrote, proving the cap check runs
        // automatically and not only when a test calls `enforce_cap`
        // directly.
        assert_eq!(
            lookup(Some(&root), SearchProvider::DuckDuckGo, "only", None, 0),
            None
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
