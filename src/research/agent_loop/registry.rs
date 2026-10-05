//! Tool dispatch for the agent loop: `search` → [`Searcher`], `fetch` → [`Fetcher`].
//!
//! Schemas, argument parsing, and result types come from `web/` (ADR-0006 §4);
//! this module only routes calls and renders results for the model. Zero
//! `trait`, zero `dyn`. Tests swap the live tools for a `#[cfg(test)]` queue.
//! The runner applies the per-call timeout around [`ToolRegistry::execute`].
//!
//! Per-run fetch dedup (#43): the live [`ToolRegistry`] remembers, for its
//! whole lifetime (one Research), every URL successfully fetched, keyed by
//! [`dedup_key`]. A repeat `fetch` of a known key skips the network entirely
//! and returns [`ToolResult::AlreadyFetched`]; a Search Hit whose key is
//! already known renders with an `(already fetched)` marker. The pointer is
//! backed by the stored page (#80): the registry keeps every fetched page
//! whole for the run, so `fetch` with the same url and a `part` is served
//! from memory, even after turn-history truncation dropped the first
//! delivery from the transcript. Pages are delivered in parts of
//! `LoopBudget::max_part_chars` (see [`page_parts`]).
//!
//! Eval record/replay (#107): an optional [`Tape`] records what `search`
//! and `fetch` returned, or serves both from a fixture dir instead of the
//! network. Eval-only, selected by env in `run_research`; see `tape.rs`.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::llm::{RequestedToolCall, ToolDef};
use crate::research::agent_loop::context::page_parts;
use crate::research::agent_loop::result::{plural_parts, FailureKind, ToolResult};
use crate::research::agent_loop::tape::{SearchOutcome, Tape};
use crate::web::fetch::tool::{
    fetch_tool, fetch_tool_schema, FetchInput, FETCH_TOOL_NAME, FETCH_TOOL_PURPOSE,
};
use crate::web::fetch::{Evidence, FetchError, Fetcher};
use crate::web::search::dedup_key;
use crate::web::search::tool::{
    parse_search_args, search_tool_schema, Searcher, SEARCH_TOOL_NAME, SEARCH_TOOL_PURPOSE,
};
use crate::web::search::types::{MergedResult, SearchInput};

enum Backend {
    Live {
        searcher: Searcher,
        fetcher: Fetcher,
    },
    #[cfg(test)]
    Queue(tokio::sync::Mutex<VecDeque<ToolResult>>),
}

/// Dispatch table for the tools one run may offer. Lives for one Research
/// (built once in `run_research`), so `fetched` is per-run memory (#43): the
/// lock is held only for map access, never across an `.await`.
pub struct ToolRegistry {
    backend: Backend,
    /// `dedup_key(url) -> stored page` for every URL successfully fetched
    /// so far this run, under both the requested and the final URL. The
    /// page is kept whole so a later `part` is served without a request.
    fetched: Mutex<HashMap<String, Arc<StoredPage>>>,
    /// Eval record/replay of the live tools (#107); `None` on normal runs.
    tape: Option<Tape>,
}
impl ToolRegistry {
    /// Live dispatch over the real web tools.
    pub fn new(searcher: Searcher, fetcher: Fetcher) -> Self {
        Self {
            backend: Backend::Live { searcher, fetcher },
            fetched: Mutex::new(HashMap::new()),
            tape: None,
        }
    }

    /// Record into or replay from an eval fixture (#107). Applies to the
    /// live backend only; `None` keeps the run live.
    pub(crate) fn with_tape(mut self, tape: Option<Tape>) -> Self {
        self.tape = tape;
        self
    }

    /// Test seam: pop canned results in wire order, ignoring arguments.
    #[cfg(test)]
    pub(crate) fn stub(results: VecDeque<ToolResult>) -> Self {
        Self {
            backend: Backend::Queue(tokio::sync::Mutex::new(results)),
            fetched: Mutex::new(HashMap::new()),
            tape: None,
        }
    }

    /// Test seam: live dispatch whose endpoints are unreachable, for
    /// argument-validation paths that must fail before any I/O.
    #[cfg(test)]
    pub(crate) fn offline() -> Self {
        let dead = "http://127.0.0.1:9/";
        Self::new(
            Searcher::with_bases(dead, dead, dead, dead, dead, dead),
            Fetcher::with_policy(
                crate::web::fetch::Obscura::new(
                    "/nonexistent/obscura".into(),
                    std::time::Duration::from_secs(1),
                ),
                crate::web::fetch::EgressPolicy::AllowPrivate,
            ),
        )
    }

    /// Tools known to this registry, in deterministic wire order.
    pub fn tool_names(&self) -> Vec<&'static str> {
        vec![SEARCH_TOOL_NAME, FETCH_TOOL_NAME]
    }

    /// One-line purposes aligned with the sysprompt roster.
    pub fn tool_purposes(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            (SEARCH_TOOL_NAME, SEARCH_TOOL_PURPOSE),
            (FETCH_TOOL_NAME, FETCH_TOOL_PURPOSE),
        ]
    }

    /// Wire schemas filtered by allowed names, in registry order.
    /// Unknown names are skipped silently: the runner's pre-flight already
    /// rejected them, and double-reporting would confuse the model.
    pub fn tool_defs(&self, allowed: &[String]) -> Vec<ToolDef> {
        [search_tool_schema(), fetch_tool_schema()]
            .into_iter()
            .filter(|schema| allowed.iter().any(|name| schema["name"] == name.as_str()))
            .map(|schema| ToolDef {
                name: schema["name"].as_str().unwrap_or_default().to_owned(),
                description: schema["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                parameters: schema["parameters"].clone(),
            })
            .collect()
    }

    /// Parse args + validate + run one call. Never panics: unknown names,
    /// bad JSON, and validator rejections all become [`ToolResult::Failed`]
    /// with [`FailureKind::Dispatch`]; executor failures use
    /// [`FailureKind::Execution`]. The runner reads `failure_kind()` instead
    /// of matching on the rendered reason text. `part_chars` is the size of
    /// one `fetch` part (`LoopBudget::max_part_chars`).
    pub async fn execute(&self, call: &RequestedToolCall, part_chars: usize) -> ToolResult {
        let (searcher, fetcher) = match &self.backend {
            Backend::Live { searcher, fetcher } => (searcher, fetcher),
            #[cfg(test)]
            Backend::Queue(queue) => {
                let next = queue.lock().await.pop_front().unwrap_or_else(|| {
                    failed(
                        call,
                        "stub queue exhausted".to_owned(),
                        FailureKind::Execution,
                    )
                });
                if matches!(next, ToolResult::Hang) {
                    std::future::pending::<()>().await;
                }
                return next;
            }
        };
        let args = match call.parsed_arguments() {
            Ok(args) => args,
            Err(err) => {
                return failed(
                    call,
                    format!("arguments are not valid JSON: {err}"),
                    FailureKind::Dispatch,
                )
            }
        };
        match call.name.as_str() {
            SEARCH_TOOL_NAME => {
                let input = match parse_search_args(&args) {
                    Ok(input) => input,
                    Err(reason) => {
                        return failed(
                            call,
                            format!("invalid args for 'search': {reason}"),
                            FailureKind::Dispatch,
                        )
                    }
                };
                match self.search(searcher, input).await {
                    SearchOutcome::Hits { hits } => ToolResult::Search {
                        hits: self.mark_already_fetched(hits),
                    },
                    SearchOutcome::Note { note } => ToolResult::SearchNote { note },
                    SearchOutcome::Blocked { detail } => ToolResult::SearchBlocked { detail },
                    SearchOutcome::Failed { reason } => {
                        failed(call, reason, FailureKind::Execution)
                    }
                }
            }
            FETCH_TOOL_NAME => {
                let input = match FetchInput::parse(&args) {
                    Ok(input) => input,
                    Err(err) => {
                        return failed(
                            call,
                            format!("invalid args for 'fetch': {err}"),
                            FailureKind::Dispatch,
                        )
                    }
                };
                let requested_key = dedup_key(&input.url);
                let known = self
                    .fetched
                    .lock()
                    .expect("fetched lock")
                    .get(&requested_key)
                    .cloned();
                if let Some(stored) = known {
                    // Known page: never the network. Without `part` it is
                    // the #43 pointer once the model has seen the page; a
                    // page no part of which was delivered yet (an earlier
                    // call asked past the end) delivers part 1 instead.
                    return match input.part {
                        None if stored.delivered.load(Ordering::Relaxed) => {
                            ToolResult::AlreadyFetched {
                                url: input.url,
                                first_url: stored.page.source_url.clone(),
                                parts: u32::try_from(
                                    page_parts(&stored.page.markdown, part_chars).len(),
                                )
                                .unwrap_or(u32::MAX),
                            }
                        }
                        part => deliver_part(call, &stored, part.unwrap_or(1), part_chars),
                    };
                }
                match self.fetch(&args, &input.url, fetcher).await {
                    Ok(evidence) => {
                        let source_key = dedup_key(&evidence.source_url);
                        let stored = {
                            let mut fetched = self.fetched.lock().expect("fetched lock");
                            // Two requested URLs can redirect to one final
                            // URL: keep the page already stored under it, so
                            // the Session gets one row per URL.
                            let stored = match fetched.get(&source_key) {
                                Some(existing) => Arc::clone(existing),
                                None => {
                                    let stored = Arc::new(StoredPage {
                                        page: Arc::new(evidence),
                                        delivered: AtomicBool::new(false),
                                    });
                                    fetched.insert(source_key, Arc::clone(&stored));
                                    stored
                                }
                            };
                            fetched.insert(requested_key, Arc::clone(&stored));
                            stored
                        };
                        deliver_part(call, &stored, input.part.unwrap_or(1), part_chars)
                    }
                    Err((reason, kind)) => failed(call, reason, kind),
                }
            }
            _ => failed(
                call,
                format!("unknown tool '{}'", call.name),
                FailureKind::Dispatch,
            ),
        }
    }

    /// One `search` call: live (recorded when taping), or replayed.
    async fn search(&self, searcher: &Searcher, input: SearchInput) -> SearchOutcome {
        if let Some(Tape::Replay(fixture)) = &self.tape {
            return fixture.search(&input);
        }
        let queries = match &self.tape {
            Some(Tape::Record(_)) => input.queries.clone(),
            _ => Vec::new(),
        };
        let outcome = match searcher.search(input).await {
            Ok(output) => match output.note {
                Some(note) => SearchOutcome::Note { note },
                None => SearchOutcome::Hits {
                    hits: output.results,
                },
            },
            Err(crate::web::search::types::SearchProviderError::AllFailed {
                failures,
                all_challenged: true,
            }) => SearchOutcome::Blocked { detail: failures },
            Err(err) => SearchOutcome::Failed {
                reason: format!("search failed: {}", search_error(&err)),
            },
        };
        if let Some(Tape::Record(recorder)) = &self.tape {
            recorder.search(queries, &outcome);
        }
        outcome
    }

    /// One network `fetch` of a page not yet known this run: live
    /// (recorded when taping), or replayed. `Err` carries the model-facing
    /// reason and its [`FailureKind`].
    async fn fetch(
        &self,
        args: &serde_json::Value,
        url: &str,
        fetcher: &Fetcher,
    ) -> Result<Evidence, (String, FailureKind)> {
        if let Some(Tape::Replay(fixture)) = &self.tape {
            return fixture.fetch(url);
        }
        let outcome = fetch_tool(args, fetcher).await.map_err(|err| match err {
            FetchError::InvalidUrl { .. } | FetchError::InvalidPart { .. } => (
                format!("invalid args for 'fetch': {err}"),
                FailureKind::Dispatch,
            ),
            err => (format!("fetch failed: {err}"), FailureKind::Execution),
        });
        if let Some(Tape::Record(recorder)) = &self.tape {
            recorder.fetch(url, &outcome);
        }
        outcome
    }

    /// Search Hits already fetched this run get an `(already fetched)`
    /// marker appended to the title (#43). `canonical_url` is already
    /// `dedup_key(display_url)` (see `dedup::merge_sources_in_order`), so
    /// only `display_url` needs re-keying.
    fn mark_already_fetched(&self, hits: Vec<MergedResult>) -> Vec<MergedResult> {
        let fetched = self.fetched.lock().expect("fetched lock");
        if fetched.is_empty() {
            return hits;
        }
        hits.into_iter()
            .map(|mut hit| {
                if fetched.contains_key(&hit.canonical_url) {
                    hit.title.push_str(" (already fetched)");
                }
                hit
            })
            .collect()
    }
}

fn failed(call: &RequestedToolCall, reason: String, kind: FailureKind) -> ToolResult {
    ToolResult::Failed {
        tool: call.name.clone(),
        reason,
        kind,
    }
}

/// A fetched page kept whole for the run (#80). `delivered` turns true when
/// a part of it first reaches the model: that call is the one the Session
/// persists the page from, even when an earlier call fetched it over the
/// network but asked for a part past the end.
struct StoredPage {
    page: Arc<Evidence>,
    delivered: AtomicBool,
}

/// Byte ranges of `markdown` for each part of at most `part_chars` chars
/// (see [`page_parts`]). Never empty.
fn part_spans(markdown: &str, part_chars: usize) -> Vec<Range<usize>> {
    let mut start = 0;
    page_parts(markdown, part_chars)
        .into_iter()
        .map(|part| {
            let span = start..start + part.len();
            start = span.end;
            span
        })
        .collect()
}

/// Build the result for `part` (1-based) of a stored page, or a Dispatch
/// failure naming the real part count when `part` is past the end. A
/// failure leaves the page undelivered.
fn deliver_part(
    call: &RequestedToolCall,
    stored: &StoredPage,
    part: u32,
    part_chars: usize,
) -> ToolResult {
    let mut spans = part_spans(&stored.page.markdown, part_chars);
    let parts = u32::try_from(spans.len()).unwrap_or(u32::MAX);
    if part > parts {
        return failed(
            call,
            format!(
                "part {part} is past the end: the page has {parts} {}",
                plural_parts(parts)
            ),
            FailureKind::Dispatch,
        );
    }
    let span = spans.swap_remove(part as usize - 1);
    ToolResult::Fetch {
        evidence: Arc::clone(&stored.page),
        span,
        part,
        parts,
        first_delivery: !stored.delivered.swap(true, Ordering::Relaxed),
    }
}

fn search_error(err: &crate::web::search::types::SearchProviderError) -> String {
    use crate::web::search::types::SearchProviderError as E;
    match err {
        E::AllFailed { failures, .. } => format!("all engines failed: {failures}"),
        other => other.code().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    mod dedup;
    mod dispatch;
    mod helpers;
    mod parts;
}
