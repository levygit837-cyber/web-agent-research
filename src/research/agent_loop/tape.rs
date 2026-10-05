//! Eval-only record/replay of the `search` and `fetch` tools (#107).
//!
//! Not part of the Harness contract (see `docs/eval.md`). Selected by env:
//!
//! - `WAR_EVAL_RECORD=<dir>`: tools run live; every `search` outcome is
//!   appended to `<dir>/search.jsonl` and every `fetch` outcome (the
//!   cleaned [`Evidence`], or the failure) to `<dir>/pages.jsonl`, keyed by
//!   [`dedup_key`] of the requested URL.
//! - `WAR_EVAL_REPLAY=<dir>`: no network. `fetch` serves the recorded page
//!   with no TTL; a URL with no recording fails the call with an
//!   `eval replay miss` reason, also printed to stderr. `search` serves the
//!   recorded outcome of a call with the same normalized query set
//!   (lower-cased, whitespace-collapsed, sorted, deduped) verbatim; any
//!   other query set gets the recorded Hits that any of its normalized
//!   queries surfaced, else the union of every recorded Hit, deduped by
//!   `canonical_url`, Hits with a recorded page first (then first-recorded
//!   order), cut to `top_k`.
//!
//! Setting both is a configuration error. The fixture files are JSON Lines,
//! appended across runs, so K recordings of one goal share one fixture.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::research::agent_loop::result::FailureKind;
use crate::web::fetch::Evidence;
use crate::web::search::dedup_key;
use crate::web::search::types::{MergedResult, SearchInput};

/// Env var naming the fixture dir to record into.
pub const RECORD_ENV: &str = "WAR_EVAL_RECORD";
/// Env var naming the fixture dir to replay from.
pub const REPLAY_ENV: &str = "WAR_EVAL_REPLAY";
/// Marker every replay-miss reason and stderr line carries.
pub const REPLAY_MISS: &str = "eval replay miss";

const SEARCH_FILE: &str = "search.jsonl";
const PAGES_FILE: &str = "pages.jsonl";

/// What one `search` call produced, as the model would see it (before the
/// per-run `(already fetched)` marker).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum SearchOutcome {
    Hits { hits: Vec<MergedResult> },
    Note { note: String },
    Blocked { detail: String },
    Failed { reason: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchEntry {
    queries: Vec<String>,
    #[serde(flatten)]
    outcome: SearchOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Dispatch,
    Execution,
}

impl From<FailureKind> for Kind {
    fn from(kind: FailureKind) -> Self {
        match kind {
            FailureKind::Dispatch => Self::Dispatch,
            FailureKind::Execution => Self::Execution,
        }
    }
}

impl From<Kind> for FailureKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Dispatch => Self::Dispatch,
            Kind::Execution => Self::Execution,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum PageOutcome {
    Page { evidence: Evidence },
    Failed { reason: String, kind: Kind },
}

#[derive(Debug, Serialize, Deserialize)]
struct PageEntry {
    key: String,
    url: String,
    #[serde(flatten)]
    outcome: PageOutcome,
}

/// The serialized form [`Recorder::fetch`] writes, borrowing the page.
#[derive(Serialize)]
struct PageEntryRef<'a> {
    key: &'a str,
    url: &'a str,
    #[serde(flatten)]
    outcome: PageOutcomeRef<'a>,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum PageOutcomeRef<'a> {
    Page { evidence: &'a Evidence },
    Failed { reason: &'a str, kind: Kind },
}

/// Record or replay mode of one run's [`ToolRegistry`](super::ToolRegistry).
pub(crate) enum Tape {
    Record(Recorder),
    Replay(Fixture),
}

impl Tape {
    /// Mode from [`RECORD_ENV`] / [`REPLAY_ENV`]; `None` when neither is
    /// set (the normal, live run). Errors name the variable and the dir.
    pub(crate) fn from_env() -> Result<Option<Self>, String> {
        let var = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        match (var(RECORD_ENV), var(REPLAY_ENV)) {
            (None, None) => Ok(None),
            (Some(_), Some(_)) => Err(format!("set only one of {RECORD_ENV} and {REPLAY_ENV}")),
            (Some(dir), None) => Recorder::open(Path::new(&dir))
                .map(|recorder| Some(Self::Record(recorder)))
                .map_err(|err| format!("{RECORD_ENV}={dir}: {err}")),
            (None, Some(dir)) => Fixture::load(Path::new(&dir))
                .map(|fixture| Some(Self::Replay(fixture)))
                .map_err(|err| format!("{REPLAY_ENV}={dir}: {err}")),
        }
    }
}

/// Lower-case, whitespace-collapsed form of one query.
fn normalize_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn query_set(queries: &[String]) -> Vec<String> {
    let mut set: Vec<String> = queries.iter().map(|q| normalize_query(q)).collect();
    set.sort();
    set.dedup();
    set
}

/// Append-only writer of one fixture dir.
pub(crate) struct Recorder {
    search: Mutex<File>,
    pages: Mutex<File>,
}

impl Recorder {
    fn open(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let open = |name: &str| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(name))
        };
        Ok(Self {
            search: Mutex::new(open(SEARCH_FILE)?),
            pages: Mutex::new(open(PAGES_FILE)?),
        })
    }

    pub(crate) fn search(&self, queries: Vec<String>, outcome: &SearchOutcome) {
        let entry = SearchEntry {
            queries,
            outcome: outcome.clone(),
        };
        append(&self.search, &entry);
    }

    pub(crate) fn fetch(&self, url: &str, outcome: &Result<Evidence, (String, FailureKind)>) {
        let key = dedup_key(url);
        let outcome = match outcome {
            Ok(evidence) => PageOutcomeRef::Page { evidence },
            Err((reason, kind)) => PageOutcomeRef::Failed {
                reason,
                kind: (*kind).into(),
            },
        };
        append(
            &self.pages,
            &PageEntryRef {
                key: &key,
                url,
                outcome,
            },
        );
    }
}

/// A failed write would surface later as a replay miss; say so now.
fn append(file: &Mutex<File>, entry: &impl Serialize) {
    let mut line = serde_json::to_string(entry).expect("tape entries serialize");
    line.push('\n');
    let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Err(err) = file.write_all(line.as_bytes()) {
        eprintln!("eval record write failed: {err}");
    }
}

/// A loaded fixture dir, read-only for the run.
pub(crate) struct Fixture {
    dir: PathBuf,
    searches: Vec<SearchEntry>,
    /// `dedup_key(requested url) -> outcome`; a recorded page wins over a
    /// recorded failure of the same key.
    pages: HashMap<String, PageOutcome>,
    /// `dedup_key(final url) -> requested key`, for a redirect target
    /// requested directly.
    finals: HashMap<String, String>,
}

impl Fixture {
    fn load(dir: &Path) -> Result<Self, String> {
        let searches: Vec<SearchEntry> = read_lines(&dir.join(SEARCH_FILE))?;
        let mut pages: HashMap<String, PageOutcome> = HashMap::new();
        let mut finals: HashMap<String, String> = HashMap::new();
        for entry in read_lines::<PageEntry>(&dir.join(PAGES_FILE))? {
            if let PageOutcome::Page { evidence } = &entry.outcome {
                finals
                    .entry(dedup_key(&evidence.source_url))
                    .or_insert_with(|| entry.key.clone());
            }
            if !matches!(pages.get(&entry.key), Some(PageOutcome::Page { .. })) {
                pages.insert(entry.key, entry.outcome);
            }
        }
        Ok(Self {
            dir: dir.to_owned(),
            searches,
            pages,
            finals,
        })
    }

    /// The replayed outcome of one `search` call (rule in the module doc).
    pub(crate) fn search(&self, input: &SearchInput) -> SearchOutcome {
        let wanted = query_set(&input.queries);
        if let Some(entry) = self
            .searches
            .iter()
            .find(|entry| query_set(&entry.queries) == wanted)
        {
            return entry.outcome.clone();
        }
        let recorded = || {
            self.searches.iter().flat_map(|entry| match &entry.outcome {
                SearchOutcome::Hits { hits } => hits.as_slice(),
                _ => &[],
            })
        };
        let wanted: HashSet<String> = wanted.into_iter().collect();
        let matching: Vec<&MergedResult> = recorded()
            .filter(|hit| {
                hit.queries
                    .iter()
                    .any(|q| wanted.contains(&normalize_query(q)))
            })
            .collect();
        let pool: Vec<&MergedResult> = if matching.is_empty() {
            recorded().collect()
        } else {
            matching
        };
        let mut seen: HashSet<&str> = HashSet::new();
        let mut pool: Vec<&MergedResult> = pool
            .into_iter()
            .filter(|hit| seen.insert(hit.canonical_url.as_str()))
            .collect();
        // Stable: recorded pages first, so a replay can read what it finds.
        pool.sort_by_key(|hit| !self.has_page(&hit.display_url));
        let hits = pool.into_iter().take(input.top_k).cloned().collect();
        SearchOutcome::Hits { hits }
    }

    fn has_page(&self, url: &str) -> bool {
        let key = dedup_key(url);
        matches!(self.pages.get(&key), Some(PageOutcome::Page { .. }))
            || self.finals.contains_key(&key)
    }

    /// The recorded page or failure of `url`; a miss is an execution
    /// failure, printed to stderr, never a network request.
    pub(crate) fn fetch(&self, url: &str) -> Result<Evidence, (String, FailureKind)> {
        let key = dedup_key(url);
        let found = self.pages.get(&key).or_else(|| {
            self.finals
                .get(&key)
                .and_then(|requested| self.pages.get(requested))
        });
        match found {
            Some(PageOutcome::Page { evidence }) => Ok(evidence.clone()),
            Some(PageOutcome::Failed { reason, kind }) => Err((reason.clone(), (*kind).into())),
            None => {
                let reason = format!(
                    "{REPLAY_MISS}: no recorded page for {url} in {}",
                    self.dir.display()
                );
                eprintln!("{reason}");
                Err((reason, FailureKind::Execution))
            }
        }
    }
}

fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .map_err(|err| format!("{}:{}: {err}", path.display(), index + 1))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::types::SearchProvider;

    fn hit(url: &str, query: &str) -> MergedResult {
        MergedResult {
            title: url.to_owned(),
            canonical_url: dedup_key(url),
            display_url: url.to_owned(),
            snippet: String::new(),
            providers: vec![SearchProvider::Bing],
            queries: vec![query.to_owned()],
            best_rank: 0,
            hit_count: 1,
            published_date: None,
        }
    }

    fn input(queries: &[&str], top_k: usize) -> SearchInput {
        SearchInput {
            queries: queries.iter().map(|q| (*q).to_owned()).collect(),
            top_k,
            recency: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("war-tape-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn recorded(tag: &str) -> PathBuf {
        let dir = temp_dir(tag);
        let recorder = Recorder::open(&dir).expect("recorder opens");
        recorder.search(
            vec!["Tokio  Runtime".to_owned()],
            &SearchOutcome::Hits {
                hits: vec![
                    hit("https://tokio.rs/", "Tokio  Runtime"),
                    hit("https://docs.rs/tokio", "Tokio  Runtime"),
                ],
            },
        );
        recorder.search(
            vec!["serde derive".to_owned()],
            &SearchOutcome::Hits {
                hits: vec![
                    hit("https://serde.rs/derive.html", "serde derive"),
                    hit("https://docs.rs/tokio", "serde derive"),
                ],
            },
        );
        recorder.search(
            vec!["walled".to_owned()],
            &SearchOutcome::Blocked {
                detail: "ddg: challenge".to_owned(),
            },
        );
        let mut page = Evidence::new(
            "https://tokio.rs/tokio/tutorial".to_owned(),
            "# Tokio".to_owned(),
        );
        page.collected_at = "2026-01-01T00:00:00Z".to_owned();
        recorder.fetch("https://www.tokio.rs/tutorial/", &Ok(page));
        recorder.fetch(
            "https://gone.example/",
            &Err(("fetch failed: 404".to_owned(), FailureKind::Execution)),
        );
        dir
    }

    #[test]
    fn same_normalized_query_set_replays_the_recorded_outcome() {
        let fixture = Fixture::load(&recorded("exact")).expect("fixture loads");
        let SearchOutcome::Hits { hits } = fixture.search(&input(&["tokio runtime"], 10)) else {
            panic!("hits");
        };
        assert_eq!(hits.len(), 2);
        assert_eq!(
            fixture.search(&input(&["WALLED"], 10)),
            SearchOutcome::Blocked {
                detail: "ddg: challenge".to_owned()
            }
        );
    }

    #[test]
    fn other_queries_get_matching_hits_then_the_deduped_union() {
        let fixture = Fixture::load(&recorded("union")).expect("fixture loads");
        let SearchOutcome::Hits { hits } =
            fixture.search(&input(&["serde derive", "something new"], 10))
        else {
            panic!("hits");
        };
        let urls: Vec<&str> = hits.iter().map(|h| h.display_url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://serde.rs/derive.html", "https://docs.rs/tokio"]
        );
        let SearchOutcome::Hits { hits } = fixture.search(&input(&["unrelated"], 2)) else {
            panic!("hits");
        };
        let urls: Vec<&str> = hits.iter().map(|h| h.display_url.as_str()).collect();
        assert_eq!(urls, ["https://tokio.rs/", "https://docs.rs/tokio"]);
    }

    #[test]
    fn fetch_serves_recorded_pages_and_failures_with_no_ttl_and_misses_loudly() {
        let dir = recorded("fetch");
        let fixture = Fixture::load(&dir).expect("fixture loads");
        let page = fixture
            .fetch("https://tokio.rs/tutorial")
            .expect("recorded page");
        assert_eq!(page.markdown, "# Tokio");
        assert_eq!(page.collected_at, "2026-01-01T00:00:00Z");
        let by_final = fixture
            .fetch("https://tokio.rs/tokio/tutorial")
            .expect("final url");
        assert_eq!(by_final.source_url, "https://tokio.rs/tokio/tutorial");
        assert_eq!(
            fixture.fetch("https://gone.example/").unwrap_err(),
            ("fetch failed: 404".to_owned(), FailureKind::Execution)
        );
        let (reason, kind) = fixture.fetch("https://unknown.example/a").unwrap_err();
        assert!(reason.starts_with(REPLAY_MISS), "{reason}");
        assert!(reason.contains("https://unknown.example/a"), "{reason}");
        assert_eq!(kind, FailureKind::Execution);
    }

    #[test]
    fn a_page_recorded_after_a_failure_wins() {
        let dir = temp_dir("wins");
        let recorder = Recorder::open(&dir).expect("recorder opens");
        recorder.fetch(
            "https://flaky.example/",
            &Err(("fetch failed: timeout".to_owned(), FailureKind::Execution)),
        );
        recorder.fetch(
            "https://flaky.example/",
            &Ok(Evidence::new(
                "https://flaky.example/".to_owned(),
                "ok".to_owned(),
            )),
        );
        let fixture = Fixture::load(&dir).expect("fixture loads");
        assert_eq!(
            fixture
                .fetch("https://flaky.example/")
                .expect("page")
                .markdown,
            "ok"
        );
    }

    #[test]
    fn a_missing_replay_dir_is_a_configuration_error() {
        let err = Fixture::load(&temp_dir("missing"))
            .err()
            .expect("missing dir fails");
        assert!(err.contains(SEARCH_FILE), "{err}");
    }
}
