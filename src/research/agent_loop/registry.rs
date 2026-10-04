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

use std::collections::HashMap;
#[cfg(test)]
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::llm::{RequestedToolCall, ToolDef};
use crate::research::agent_loop::context::page_parts;
use crate::web::fetch::tool::{
    fetch_tool, fetch_tool_schema, FetchInput, FETCH_TOOL_NAME, FETCH_TOOL_PURPOSE,
};
use crate::web::fetch::{Evidence, FetchError, FetchPath, Fetcher};
use crate::web::search::dedup_key;
use crate::web::search::tool::{
    parse_search_args, search_tool_schema, Searcher, SEARCH_TOOL_NAME, SEARCH_TOOL_PURPOSE,
};
use crate::web::search::types::MergedResult;

/// Whether a [`ToolResult::Failed`] came from validating the call itself
/// (bad name/JSON/args, never reaching the executor) or from the executor
/// actually running and failing (search/fetch error, timeout, exhausted
/// stub). The runner uses this to pick `LoopError::InvalidToolCall` vs.
/// `LoopError::ToolFailed` without re-parsing rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Dispatch,
    Execution,
}

/// One executed tool call, rendered into the tool-role transcript.
#[derive(Debug, Clone)]
pub enum ToolResult {
    /// Hits are candidates: rendered for the model, never Evidence.
    Search { hits: Vec<MergedResult> },
    /// The `search` call ran no leg on purpose (#81: `recency` set, no
    /// enabled engine applies it). Success, not Hits and not a failure:
    /// the note tells the model to search again without `recency`.
    SearchNote { note: String },
    /// One part of a fetched page (#80). `evidence` is the whole stored
    /// page; `span` is the byte range of `evidence.markdown` this result
    /// delivers, `part` of `parts` (1-based). `first_delivery` is true only
    /// when this call fetched the page over the network.
    Fetch {
        evidence: Arc<Evidence>,
        span: Range<usize>,
        part: u32,
        parts: u32,
        first_delivery: bool,
    },
    /// A repeat `fetch` of a URL already fetched in this Research (#43)
    /// with no `part`: no network I/O ran. `url` is the requested URL for
    /// this call; `first_url` is the canonical `Evidence.source_url` from
    /// the earlier fetch; `parts` is how many parts the stored page has.
    /// Success, not Evidence: `url()` is `None`.
    AlreadyFetched {
        url: String,
        first_url: String,
        parts: u32,
    },
    /// Every Startpage/DuckDuckGo leg of this `search` call was a bot-wall
    /// Challenge (#53), i.e. `SearchProviderError::AllFailed { all_challenged: true, .. }`:
    /// the request never reached a usable results page. Distinct from the
    /// generic `Failed` so the runner can remember it across the whole run
    /// (`RunReport::search_blocked`) even though it counts the same as an
    /// execution failure for the repair budget. `detail` carries the
    /// per-engine challenge messages (Omp `formatSearchProviderFailures`).
    SearchBlocked { detail: String },
    #[cfg(test)]
    Hang,
    Failed {
        tool: String,
        reason: String,
        kind: FailureKind,
    },
}

impl ToolResult {
    /// Render one call's result into observation text. Uncapped pure
    /// rendering; the runner caps every result except `Fetch`.
    pub fn render(&self) -> String {
        match self {
            Self::SearchNote { note } => note.clone(),
            Self::Search { hits } => {
                if hits.is_empty() {
                    return "No Hits for these queries. Rephrase once with different words; if that still finds nothing, answer from the pages you already fetched and say what is missing.".to_owned();
                }
                let noun = if hits.len() == 1 { "Hit" } else { "Hits" };
                let mut text = format!(
                    "{} {noun}. Candidates only, not Evidence: fetch a URL before stating or citing anything from its snippet.",
                    hits.len()
                );
                for (index, hit) in hits.iter().enumerate() {
                    text.push_str(&format!(
                        "\n{}. {}\n   {}",
                        index + 1,
                        hit.title,
                        hit.display_url
                    ));
                    if !hit.snippet.is_empty() {
                        text.push_str(&format!("\n   {}", hit.snippet));
                    }
                }
                text
            }
            // Source first: the model needs the final URL to cite. The
            // runner never caps a Fetch result; the part is already sized.
            Self::Fetch {
                evidence,
                span,
                part,
                parts,
                ..
            } => {
                let mut text = format!("Source: {}\n", evidence.source_url);
                if *parts > 1 {
                    text.push_str(&format!("Part {part} of {parts}\n"));
                }
                text.push('\n');
                text.push_str(&evidence.markdown[span.clone()]);
                if part < parts {
                    text.push_str(&format!(
                        "\n[part {part} of {parts}; call fetch with the same url and part={} for the rest]",
                        part + 1
                    ));
                }
                text
            }
            Self::AlreadyFetched {
                url,
                first_url,
                parts,
            } => {
                let mut text = format!(
                    "ALREADY FETCHED: {url} was already fetched earlier in this Research (as {first_url}). Use that earlier content, or fetch a different Hit."
                );
                text.push_str(&format!(
                    " The page has {parts} {}; call fetch with the same url and part=k to read part k again (served from memory, no new request).",
                    plural_parts(*parts)
                ));
                text
            }
            Self::SearchBlocked { detail } => format!(
                "FAILED: search blocked: {detail}\nEvery enabled search engine answered with a bot-detection wall, so searching again in this run fails the same way. Answer from pages you already fetched, or fetch URLs named in the research goal."
            ),
            #[cfg(test)]
            Self::Hang => "hanging".to_owned(),
            Self::Failed { tool, reason, kind } => {
                if tool == FETCH_TOOL_NAME && *kind == FailureKind::Execution {
                    format!("FAILED: {reason}\nThis page could not be read, so it is not Evidence: fetch a different Hit instead of retrying this URL.")
                } else {
                    format!("FAILED: {reason}")
                }
            }
        }
    }

    /// Source URL of fetched Evidence. Search Hits and `AlreadyFetched`
    /// carry none: neither is Evidence.
    pub fn url(&self) -> Option<String> {
        match self {
            Self::Fetch { evidence, .. } => Some(evidence.source_url.clone()),
            _ => None,
        }
    }

    /// Which engine produced the fetched Evidence (#31); `None` for every
    /// non-`Fetch` result.
    pub fn fetch_path(&self) -> Option<FetchPath> {
        match self {
            Self::Fetch { evidence, .. } => evidence.fetch_path,
            _ => None,
        }
    }

    /// The whole page, when this result is the first time it reached the
    /// model this run: the one the Session persists (#80). Continuation
    /// parts and repeats return `None`.
    pub fn first_delivery_page(&self) -> Option<Arc<Evidence>> {
        match self {
            Self::Fetch {
                evidence,
                first_delivery: true,
                ..
            } => Some(Arc::clone(evidence)),
            _ => None,
        }
    }

    /// Execution worked (even an empty hit list, or a skipped repeat fetch,
    /// is information, not failure). `SearchBlocked` is not: the bot wall
    /// stopped the call from reaching a usable page.
    pub fn is_success(&self) -> bool {
        !matches!(self, Self::Failed { .. } | Self::SearchBlocked { .. })
    }

    /// `Some` naming which kind of failure this is; `None` for a success.
    /// `SearchBlocked` executed and failed at the provider, same as any
    /// other search execution error.
    pub fn failure_kind(&self) -> Option<FailureKind> {
        match self {
            Self::Failed { kind, .. } => Some(*kind),
            Self::SearchBlocked { .. } => Some(FailureKind::Execution),
            _ => None,
        }
    }
}

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
}
impl ToolRegistry {
    /// Live dispatch over the real web tools.
    pub fn new(searcher: Searcher, fetcher: Fetcher) -> Self {
        Self {
            backend: Backend::Live { searcher, fetcher },
            fetched: Mutex::new(HashMap::new()),
        }
    }

    /// Test seam: pop canned results in wire order, ignoring arguments.
    #[cfg(test)]
    pub(crate) fn stub(results: VecDeque<ToolResult>) -> Self {
        Self {
            backend: Backend::Queue(tokio::sync::Mutex::new(results)),
            fetched: Mutex::new(HashMap::new()),
        }
    }

    /// Test seam: live dispatch whose endpoints are unreachable, for
    /// argument-validation paths that must fail before any I/O.
    #[cfg(test)]
    pub(crate) fn offline() -> Self {
        let dead = "http://127.0.0.1:9/";
        Self::new(
            Searcher::with_bases(dead, dead, dead, dead, dead, dead),
            Fetcher::with_obscura(crate::web::fetch::Obscura::new(
                "/nonexistent/obscura".into(),
                std::time::Duration::from_secs(1),
            )),
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
                match searcher.search(input).await {
                    Ok(output) => match output.note {
                        Some(note) => ToolResult::SearchNote { note },
                        None => ToolResult::Search {
                            hits: self.mark_already_fetched(output.results),
                        },
                    },
                    Err(crate::web::search::types::SearchProviderError::AllFailed {
                        failures,
                        all_challenged: true,
                    }) => ToolResult::SearchBlocked { detail: failures },
                    Err(err) => failed(
                        call,
                        format!("search failed: {}", search_error(&err)),
                        FailureKind::Execution,
                    ),
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
                match fetch_tool(&args, fetcher).await {
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
                    Err(err @ (FetchError::InvalidUrl { .. } | FetchError::InvalidPart { .. })) => {
                        failed(
                            call,
                            format!("invalid args for 'fetch': {err}"),
                            FailureKind::Dispatch,
                        )
                    }
                    Err(err) => {
                        failed(call, format!("fetch failed: {err}"), FailureKind::Execution)
                    }
                }
            }
            _ => failed(
                call,
                format!("unknown tool '{}'", call.name),
                FailureKind::Dispatch,
            ),
        }
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

fn plural_parts(parts: u32) -> &'static str {
    if parts == 1 {
        "part"
    } else {
        "parts"
    }
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

/// Test helper: one Hit with the given title and URL.
#[cfg(test)]
pub(crate) fn test_hit(title: &str, url: &str) -> MergedResult {
    MergedResult {
        title: title.to_owned(),
        canonical_url: url.to_owned(),
        display_url: url.to_owned(),
        snippet: "S".to_owned(),
        providers: vec![],
        queries: vec![],
        best_rank: 0,
        hit_count: 1,
        published_date: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Part size for tests that do not care about splitting.
    const TEST_PART_CHARS: usize = 24_000;

    fn call(name: &str, arguments: &str) -> RequestedToolCall {
        RequestedToolCall {
            id: "call_1".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    fn reason(result: ToolResult) -> String {
        match result {
            ToolResult::Failed { reason, .. } => reason,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn kind(result: &ToolResult) -> FailureKind {
        match result {
            ToolResult::Failed { kind, .. } => *kind,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn tool_defs_come_from_web_schemas_in_registry_order() {
        let registry = ToolRegistry::offline();
        let defs = registry.tool_defs(&["fetch".to_owned(), "search".to_owned()]);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "search");
        assert_eq!(defs[0].parameters, search_tool_schema()["parameters"]);
        assert_eq!(defs[1].name, "fetch");
        assert_eq!(defs[1].parameters, fetch_tool_schema()["parameters"]);
        assert_eq!(registry.tool_defs(&["search".to_owned()]).len(), 1);
        assert!(registry.tool_defs(&["nope".to_owned()]).is_empty());
    }

    #[tokio::test]
    async fn stub_pops_in_order_then_exhausts() {
        let registry = ToolRegistry::stub(VecDeque::from([ToolResult::Search { hits: vec![] }]));
        assert!(matches!(
            registry
                .execute(&call("search", "{}"), TEST_PART_CHARS)
                .await,
            ToolResult::Search { .. }
        ));
        assert!(reason(
            registry
                .execute(&call("search", "{}"), TEST_PART_CHARS)
                .await
        )
        .contains("exhausted"));
    }

    #[tokio::test]
    async fn dispatch_failures_are_classified_before_io() {
        let registry = ToolRegistry::offline();
        let unknown = registry
            .execute(&call("Search", "{}"), TEST_PART_CHARS)
            .await;
        assert_eq!(kind(&unknown), FailureKind::Dispatch);
        assert!(reason(unknown).contains("unknown tool"));
        let bad_json = registry
            .execute(&call("search", "{bad"), TEST_PART_CHARS)
            .await;
        assert_eq!(kind(&bad_json), FailureKind::Dispatch);
        assert!(reason(bad_json).contains("not valid JSON"));
        let bad_args = registry
            .execute(&call("search", r#"{"query": "x"}"#), TEST_PART_CHARS)
            .await;
        assert_eq!(kind(&bad_args), FailureKind::Dispatch);
        assert!(reason(bad_args).starts_with("invalid args for 'search'"));
        let bad_url = registry
            .execute(&call("fetch", r#"{"url": "ftp://x/y"}"#), TEST_PART_CHARS)
            .await;
        assert_eq!(kind(&bad_url), FailureKind::Dispatch);
        assert!(reason(bad_url).starts_with("invalid args for 'fetch'"));
    }

    #[tokio::test]
    async fn unreachable_endpoints_are_execution_failures() {
        let registry = ToolRegistry::offline();
        let search = registry
            .execute(&call("search", r#"{"queries": ["x"]}"#), TEST_PART_CHARS)
            .await;
        assert_eq!(kind(&search), FailureKind::Execution);
        assert!(reason(search).starts_with("search failed"));
        let fetch = registry
            .execute(
                &call("fetch", r#"{"url": "http://127.0.0.1:9/page"}"#),
                TEST_PART_CHARS,
            )
            .await;
        assert_eq!(kind(&fetch), FailureKind::Execution);
        assert!(reason(fetch).starts_with("fetch failed"));
    }

    #[test]
    fn only_fetch_results_carry_evidence_urls() {
        let search = ToolResult::Search {
            hits: vec![test_hit("T", "https://example.com/t")],
        };
        assert_eq!(search.url(), None);
        let fetch = ToolResult::Fetch {
            evidence: Arc::new(Evidence::new(
                "https://example.com/p".to_owned(),
                "Body".to_owned(),
            )),
            span: 0..4,
            part: 1,
            parts: 1,
            first_delivery: true,
        };
        assert_eq!(fetch.url().as_deref(), Some("https://example.com/p"));
    }

    /// The answer parser turns inline `[title](url)` links into citations,
    /// so a Hit rendered in that syntax invites the model to copy it as a
    /// citation for a page it never read (#72): Hits list plain URLs.
    #[test]
    fn hits_never_render_as_citation_links() {
        let rendered = ToolResult::Search {
            hits: vec![test_hit("T", "https://example.com/t")],
        }
        .render();
        assert!(rendered.contains("https://example.com/t"), "{rendered}");
        assert!(!rendered.contains("]("), "{rendered}");
    }

    /// Citations survive only when they match the fetched page's final URL,
    /// so every part must lead with it, and a part that is not the last
    /// must say how to get the next one.
    #[test]
    fn every_part_leads_with_the_source_url_and_points_to_the_next() {
        let page = Arc::new(Evidence::new(
            "https://example.com/final".to_owned(),
            "aaaabbbbcc".to_owned(),
        ));
        let render = |span: Range<usize>, part: u32| {
            ToolResult::Fetch {
                evidence: Arc::clone(&page),
                span,
                part,
                parts: 3,
                first_delivery: true,
            }
            .render()
        };
        let first = render(0..4, 1);
        assert!(first.starts_with("Source: https://example.com/final\n"));
        assert!(first.contains("aaaa") && !first.contains("bbbb"));
        assert!(first.contains("part=2"), "{first}");
        let last = render(8..10, 3);
        assert!(last.starts_with("Source: https://example.com/final\n"));
        assert!(last.contains("cc") && !last.contains("bbbb"));
        assert!(!last.contains("part="), "the last part has no next: {last}");
    }

    #[test]
    fn a_single_part_page_renders_without_part_markers() {
        let page = Arc::new(Evidence::new(
            "https://example.com/p".to_owned(),
            "Body".to_owned(),
        ));
        let rendered = ToolResult::Fetch {
            evidence: page,
            span: 0..4,
            part: 1,
            parts: 1,
            first_delivery: true,
        }
        .render();
        assert!(rendered.starts_with("Source: https://example.com/p\n"));
        assert!(rendered.ends_with("Body"));
        assert!(!rendered.contains("part"), "{rendered}");
        assert!(!rendered.contains("Part"), "{rendered}");
    }

    fn page_html() -> String {
        format!(
            "<html><body><h1>Obscura</h1><p>{}</p></body></html>",
            "Obscura is a headless browser written in Rust. ".repeat(8)
        )
    }

    /// Last colon-separated segment of a `http://host:port` base URL.
    fn base_port(base: &str) -> &str {
        base.rsplit(':').next().unwrap_or_default()
    }

    #[tokio::test]
    async fn repeat_fetch_skips_network_and_returns_already_fetched() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
        let url = format!("{}/page", pages.base());
        let registry = ToolRegistry::offline();

        let first = registry
            .execute(
                &call("fetch", &serde_json::json!({ "url": url }).to_string()),
                TEST_PART_CHARS,
            )
            .await;
        let evidence_url = match first {
            ToolResult::Fetch { evidence, .. } => evidence.source_url.clone(),
            other => panic!("expected Fetch, got {other:?}"),
        };
        assert_eq!(pages.hits("/"), 1);

        let second = registry
            .execute(
                &call("fetch", &serde_json::json!({ "url": url }).to_string()),
                TEST_PART_CHARS,
            )
            .await;
        match &second {
            ToolResult::AlreadyFetched {
                url: repeated,
                first_url,
                ..
            } => {
                assert_eq!(repeated, &url);
                assert_eq!(first_url, &evidence_url);
            }
            other => panic!("expected AlreadyFetched, got {other:?}"),
        }
        assert_eq!(second.url(), None, "AlreadyFetched carries no Evidence URL");
        assert!(
            second.is_success(),
            "a skipped repeat is success, not a repair failure"
        );
        assert!(second.render().contains("ALREADY FETCHED"));
        assert_eq!(pages.hits("/"), 1, "repeat fetch must not reach the server");
    }

    #[tokio::test]
    async fn www_and_trailing_slash_variants_dedup() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
        let base = pages.base();
        let port = base_port(&base);
        let first_url = format!("http://localhost:{port}/page");
        let second_url = format!("http://www.localhost:{port}/page/");
        let registry = ToolRegistry::offline();

        let first = registry
            .execute(
                &call(
                    "fetch",
                    &serde_json::json!({ "url": first_url }).to_string(),
                ),
                TEST_PART_CHARS,
            )
            .await;
        assert!(matches!(first, ToolResult::Fetch { .. }));
        assert_eq!(pages.hits("/"), 1);

        let second = registry
            .execute(
                &call(
                    "fetch",
                    &serde_json::json!({ "url": second_url }).to_string(),
                ),
                TEST_PART_CHARS,
            )
            .await;
        assert!(
            matches!(second, ToolResult::AlreadyFetched { .. }),
            "expected AlreadyFetched, got {second:?}"
        );
        assert_eq!(
            pages.hits("/"),
            1,
            "the www/trailing-slash variant must dedup before any network I/O"
        );
    }

    #[tokio::test]
    async fn redirect_target_dedups_against_final_url() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages = StubServer::serve(|path: &str, _: &str| {
            if path == "/from" {
                StubReply::redirect(301, "/to")
            } else {
                StubReply::text(200, &page_html())
            }
        })
        .await;
        let from_url = format!("{}/from", pages.base());
        let to_url = format!("{}/to", pages.base());
        let registry = ToolRegistry::offline();

        let first = registry
            .execute(
                &call("fetch", &serde_json::json!({ "url": from_url }).to_string()),
                TEST_PART_CHARS,
            )
            .await;
        let final_url = match first {
            ToolResult::Fetch { evidence, .. } => evidence.source_url.clone(),
            other => panic!("expected Fetch, got {other:?}"),
        };
        assert_eq!(
            final_url, to_url,
            "Evidence.source_url must be the post-redirect URL"
        );

        let second = registry
            .execute(
                &call("fetch", &serde_json::json!({ "url": to_url }).to_string()),
                TEST_PART_CHARS,
            )
            .await;
        match second {
            ToolResult::AlreadyFetched { first_url, .. } => assert_eq!(first_url, final_url),
            other => panic!(
                "fetching the redirect target directly must dedup against the earlier final URL, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn failed_fetch_does_not_record_and_allows_retry() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(404, "nope")).await;
        let url = format!("{}/missing", pages.base());
        let registry = ToolRegistry::offline();

        let first = registry
            .execute(
                &call("fetch", &serde_json::json!({ "url": url }).to_string()),
                TEST_PART_CHARS,
            )
            .await;
        assert!(matches!(first, ToolResult::Failed { .. }));
        assert_eq!(pages.hits("/"), 1);

        let second = registry
            .execute(
                &call("fetch", &serde_json::json!({ "url": url }).to_string()),
                TEST_PART_CHARS,
            )
            .await;
        assert!(
            matches!(second, ToolResult::Failed { .. }),
            "a failed fetch must not dedup a retry, got {second:?}"
        );
        assert_eq!(
            pages.hits("/"),
            2,
            "a failed fetch is never recorded, so a retry hits the server again"
        );
    }

    #[tokio::test]
    async fn already_fetched_hit_gets_marker_others_dont() {
        use crate::web::search::test_support::{
            ddg_rows, sp_home_form, sp_rows, FanoutStub, StubReply, StubServer,
        };
        let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
        let url_a = format!("{}/a", pages.base());
        let url_b = "https://example.com/never-fetched".to_owned();

        let (search_base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            ddg_rows(&url_a, "A title", "a snippet"),
            sp_home_form(),
            sp_rows(&url_b, "B title", "b snippet"),
            false,
        ))
        .await;
        let registry = ToolRegistry::new(
            Searcher::with_bases(
                &format!("{search_base}/html/"),
                &format!("{search_base}/"),
                &format!("{search_base}/sp/search"),
                &format!("{search_base}/search"),
                &format!("{search_base}/search"),
                &format!("{search_base}/search"),
            ),
            Fetcher::with_obscura(crate::web::fetch::Obscura::new(
                "/nonexistent/obscura".into(),
                std::time::Duration::from_secs(1),
            )),
        );

        let fetched = registry
            .execute(
                &call("fetch", &serde_json::json!({ "url": url_a }).to_string()),
                TEST_PART_CHARS,
            )
            .await;
        assert!(matches!(fetched, ToolResult::Fetch { .. }));

        let searched = registry
            .execute(
                &call(
                    "search",
                    &serde_json::json!({ "queries": ["obscura"] }).to_string(),
                ),
                TEST_PART_CHARS,
            )
            .await;
        let rendered = searched.render();
        assert!(
            rendered.contains("A title (already fetched)"),
            "fetched Hit must carry the marker: {rendered}"
        );
        assert!(
            rendered.contains("B title") && !rendered.contains("B title (already fetched)"),
            "never-fetched Hit must render without the marker: {rendered}"
        );
    }

    /// A long page: 1,000 numbered paragraphs, about 60K chars of markdown.
    fn long_page_html() -> String {
        let paragraphs: String = (0..1_000)
            .map(|i| format!("<p>Paragraph {i:04}: filler words to make the page long enough.</p>"))
            .collect();
        format!("<html><body><h1>Long</h1>{paragraphs}</body></html>")
    }

    async fn fetch_part(registry: &ToolRegistry, url: &str, part: Option<u32>) -> ToolResult {
        let args = match part {
            Some(part) => serde_json::json!({ "url": url, "part": part }),
            None => serde_json::json!({ "url": url }),
        };
        registry
            .execute(&call("fetch", &args.to_string()), TEST_PART_CHARS)
            .await
    }

    /// The slice of the stored page a `Fetch` result delivers.
    fn delivered(result: &ToolResult) -> &str {
        match result {
            ToolResult::Fetch { evidence, span, .. } => &evidence.markdown[span.clone()],
            other => panic!("expected Fetch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn long_page_comes_in_parts_served_from_memory() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages =
            StubServer::serve(|_: &str, _: &str| StubReply::text(200, &long_page_html())).await;
        let url = format!("{}/long", pages.base());
        let registry = ToolRegistry::offline();

        let first = fetch_part(&registry, &url, None).await;
        let page = match &first {
            ToolResult::Fetch {
                evidence,
                part: 1,
                parts: 3,
                first_delivery: true,
                ..
            } => Arc::clone(evidence),
            other => panic!("expected part 1 of 3 on first delivery, got {other:?}"),
        };
        let chars = page.markdown.chars().count();
        assert!(
            chars > 2 * TEST_PART_CHARS && chars <= 3 * TEST_PART_CHARS,
            "the fixture must be a 3-part page, got {chars} chars"
        );
        assert_eq!(pages.hits("/"), 1);
        let rendered = first.render();
        assert!(rendered.starts_with(&format!("Source: {}\n", page.source_url)));
        assert!(
            rendered.contains("part=2"),
            "a part that is not the last must say how to get the next: {}",
            &rendered[rendered.len().saturating_sub(120)..]
        );

        let second = fetch_part(&registry, &url, Some(2)).await;
        let third = fetch_part(&registry, &url, Some(3)).await;
        for (result, expected_part) in [(&second, 2), (&third, 3)] {
            match result {
                ToolResult::Fetch {
                    part,
                    parts: 3,
                    first_delivery: false,
                    ..
                } => assert_eq!(*part, expected_part),
                other => panic!("expected a continuation of part {expected_part}, got {other:?}"),
            }
        }
        assert_eq!(
            pages.hits("/"),
            1,
            "continuation parts must be served from memory"
        );
        assert!(
            !third.render().contains("part="),
            "the last part has no next"
        );

        let joined = [delivered(&first), delivered(&second), delivered(&third)].concat();
        assert_eq!(joined, page.markdown, "the parts must tile the page");
    }

    /// A `part` past the end fails with the page's real part count. The
    /// model has seen nothing, so the page stays undelivered: the next
    /// valid call is its first delivery (the one the Session persists) and
    /// costs no second request.
    #[tokio::test]
    async fn part_past_the_end_fails_and_leaves_the_page_undelivered() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages =
            StubServer::serve(|_: &str, _: &str| StubReply::text(200, &long_page_html())).await;
        let url = format!("{}/long", pages.base());
        let registry = ToolRegistry::offline();

        let past = fetch_part(&registry, &url, Some(9)).await;
        assert_eq!(kind(&past), FailureKind::Dispatch);
        assert!(past.first_delivery_page().is_none());
        let failure = reason(past);
        assert!(
            failure.contains("3"),
            "the failure must name the part count: {failure}"
        );
        assert_eq!(pages.hits("/"), 1, "the failed call still fetched the page");

        // No part was delivered, so a plain repeat delivers part 1 now
        // instead of pointing at content the model never saw.
        let first = fetch_part(&registry, &url, None).await;
        assert!(
            matches!(
                first,
                ToolResult::Fetch {
                    part: 1,
                    parts: 3,
                    first_delivery: true,
                    ..
                }
            ),
            "got {first:?}"
        );
        assert!(first.first_delivery_page().is_some());

        let four = fetch_part(&registry, &url, Some(4)).await;
        assert_eq!(kind(&four), FailureKind::Dispatch);
        assert!(reason(four).contains("3"));
        let later = fetch_part(&registry, &url, Some(2)).await;
        assert!(later.first_delivery_page().is_none(), "already delivered");
        assert_eq!(pages.hits("/"), 1);
    }

    /// After a part reached the model, a repeat without `part` is the #43
    /// pointer and tells the model how many parts the page has.
    #[tokio::test]
    async fn repeat_without_part_points_at_the_parts() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages =
            StubServer::serve(|_: &str, _: &str| StubReply::text(200, &long_page_html())).await;
        let url = format!("{}/long", pages.base());
        let registry = ToolRegistry::offline();

        fetch_part(&registry, &url, None).await;
        let repeat = fetch_part(&registry, &url, None).await;
        assert!(
            matches!(repeat, ToolResult::AlreadyFetched { parts: 3, .. }),
            "got {repeat:?}"
        );
        let rendered = repeat.render();
        assert!(rendered.contains("ALREADY FETCHED"), "{rendered}");
        assert!(
            rendered.contains('3'),
            "the part count must be stated: {rendered}"
        );
        assert_eq!(pages.hits("/"), 1);
    }

    /// Two requested URLs that redirect to one final URL are one page: the
    /// second call must not become a second first delivery (a second
    /// Session row for the same `source_url`), and its parts come from the
    /// page already stored.
    #[tokio::test]
    async fn two_urls_redirecting_to_one_page_deliver_it_once() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages = StubServer::serve(|path: &str, _: &str| match path {
            "/final" => StubReply::text(200, &long_page_html()),
            _ => StubReply::redirect(302, "/final"),
        })
        .await;
        let registry = ToolRegistry::offline();

        let via_a = fetch_part(&registry, &format!("{}/a", pages.base()), None).await;
        let via_b = fetch_part(&registry, &format!("{}/b", pages.base()), None).await;
        let (
            ToolResult::Fetch {
                evidence: page_a, ..
            },
            ToolResult::Fetch {
                evidence: page_b, ..
            },
        ) = (&via_a, &via_b)
        else {
            panic!("expected two Fetch results, got {via_a:?} and {via_b:?}");
        };
        assert_eq!(page_a.source_url, page_b.source_url);
        assert!(Arc::ptr_eq(page_a, page_b), "one stored page per final URL");
        assert!(via_a.first_delivery_page().is_some());
        assert!(
            via_b.first_delivery_page().is_none(),
            "the same page must not be persisted twice"
        );
    }

    #[tokio::test]
    async fn first_fetch_can_ask_for_a_later_part_and_a_bad_part_is_rejected_offline() {
        use crate::web::search::test_support::{StubReply, StubServer};
        let pages =
            StubServer::serve(|_: &str, _: &str| StubReply::text(200, &long_page_html())).await;
        let url = format!("{}/long", pages.base());
        let registry = ToolRegistry::offline();

        let zero = fetch_part(&registry, &url, Some(0)).await;
        assert_eq!(kind(&zero), FailureKind::Dispatch);
        assert_eq!(
            pages.hits("/"),
            0,
            "a bad part must fail before any request"
        );

        let second = fetch_part(&registry, &url, Some(2)).await;
        assert!(
            matches!(
                second,
                ToolResult::Fetch {
                    part: 2,
                    parts: 3,
                    first_delivery: true,
                    ..
                }
            ),
            "got {second:?}"
        );
        assert!(
            second.first_delivery_page().is_some(),
            "a network fetch is the page's first delivery whatever part it asked for"
        );
    }

    fn llm_tool_call(id: &str, name: &str, arguments: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": arguments}
        })
    }

    fn llm_tools_body(calls: Vec<serde_json::Value>) -> String {
        serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "", "tool_calls": calls},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string()
    }

    fn llm_text_body(text: &str) -> String {
        serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string()
    }

    fn read_llm_request(stream: &mut std::net::TcpStream) {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let Ok(n) = stream.read(&mut chunk) else {
                return;
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(buf.len(), |i| i + 4);
        let content_length = String::from_utf8_lossy(&buf[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        let mut remaining = content_length.saturating_sub(buf.len() - header_end);
        while remaining > 0 {
            let Ok(n) = stream.read(&mut chunk) else {
                return;
            };
            if n == 0 {
                return;
            }
            remaining = remaining.saturating_sub(n);
        }
    }

    fn spawn_llm_double(responses: Vec<String>) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        let addr = listener.local_addr().expect("listener has an address");
        std::thread::spawn(move || {
            for body in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                read_llm_request(&mut stream);
                let head = format!(
                    "HTTP/1.1 200 x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
            }
        });
        format!("http://{addr}/v1")
    }

    /// Acceptance (#43): a hermetic full-turn run where the model requests
    /// `fetch` on the same URL twice yields exactly one entry with a URL in
    /// `RunReport.evidence` (the shape `run.rs`'s `evidence_urls` derives from).
    #[tokio::test]
    async fn full_run_dedups_repeat_fetch_into_one_evidence_url() {
        use crate::llm::{Gateway, GatewayConfig};
        use crate::research::agent_loop::runner::{run_loop, LoopBudget, LoopInput};
        use crate::research::synthesis::SynthesisSize;
        use crate::web::search::test_support::{StubReply, StubServer};

        let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
        let page_url = format!("{}/obscura", pages.base());

        let registry = ToolRegistry::new(
            Searcher::with_bases(
                "http://127.0.0.1:9/",
                "http://127.0.0.1:9/",
                "http://127.0.0.1:9/",
                "http://127.0.0.1:9/",
                "http://127.0.0.1:9/",
                "http://127.0.0.1:9/",
            ),
            Fetcher::with_obscura(crate::web::fetch::Obscura::new(
                "/nonexistent/obscura".into(),
                std::time::Duration::from_secs(1),
            )),
        );

        let base_url = spawn_llm_double(vec![
            llm_tools_body(vec![llm_tool_call(
                "c1",
                "fetch",
                &serde_json::json!({ "url": page_url }).to_string(),
            )]),
            llm_tools_body(vec![llm_tool_call(
                "c2",
                "fetch",
                &serde_json::json!({ "url": page_url }).to_string(),
            )]),
            llm_text_body(&format!(
                "Summary about {page_url}.\n\n## Findings\n\n- Point one.\n"
            )),
        ]);
        let gateway = Gateway::with_client(
            GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
                .with_max_attempts(1),
            reqwest::Client::new(),
        );
        let input = LoopInput {
            goal: "What is the Obscura engine?".to_owned(),
            size: SynthesisSize::Medium,
            allowed_tools: vec!["fetch".to_owned()],
        };
        let report = run_loop(&gateway, &registry, &input, &LoopBudget::default())
            .await
            .expect("run must finalize");
        let evidence_urls: Vec<String> = report
            .evidence
            .iter()
            .filter_map(|e| e.url.clone())
            .collect();
        assert_eq!(evidence_urls, vec![page_url]);
    }

    /// Regression (#53): both provider legs walled with a bot-detection
    /// challenge -> `searcher.search` returns
    /// `SearchProviderError::AllFailed { all_challenged: true, .. }`, and
    /// `execute` must turn that into `ToolResult::SearchBlocked`, not the
    /// generic `Failed`, so the runner can remember it for the whole run.
    /// #66: `Searcher`'s hermetic `Governor` always enables all five
    /// providers, so Brave/Yahoo/Bing each get their own tiny stub tuned to
    /// their real Challenge marker (Brave/Bing: HTTP 429; Yahoo: a redirect
    /// to a `guce.yahoo.com` host) so every enabled engine stays walled.
    #[tokio::test]
    async fn all_challenged_search_becomes_search_blocked() {
        use crate::web::search::test_support::{sp_home_form, FanoutStub, StubReply, StubServer};
        let (search_base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            r#"<div id="anomaly-modal"></div>"#.to_string(),
            sp_home_form(),
            r#"<script id="anubis_challenge" type="application/json">{}</script>"#.to_string(),
            false,
        ))
        .await;
        let brave_stub =
            StubServer::serve(|_: &str, _: &str| StubReply::text(429, "rate limited")).await;
        let yahoo_stub = StubServer::serve(|_: &str, _: &str| {
            StubReply::redirect(302, "https://guce.yahoo.com/consent")
        })
        .await;
        let bing_stub = StubServer::serve(|_: &str, _: &str| StubReply::text(429, "blocked")).await;
        let registry = ToolRegistry::new(
            Searcher::with_bases(
                &format!("{search_base}/html/"),
                &format!("{search_base}/"),
                &format!("{search_base}/sp/search"),
                &brave_stub.base(),
                &yahoo_stub.base(),
                &bing_stub.base(),
            ),
            Fetcher::with_obscura(crate::web::fetch::Obscura::new(
                "/nonexistent/obscura".into(),
                std::time::Duration::from_secs(1),
            )),
        );
        let result = registry
            .execute(&call("search", r#"{"queries": ["q"]}"#), TEST_PART_CHARS)
            .await;
        match &result {
            ToolResult::SearchBlocked { detail } => {
                assert!(
                    detail.contains("startpage") && detail.contains("duckduckgo"),
                    "detail must carry both engines' challenge messages: {detail}"
                );
            }
            other => panic!("expected SearchBlocked, got {other:?}"),
        }
        assert!(!result.is_success(), "SearchBlocked must not be a success");
        assert_eq!(result.failure_kind(), Some(FailureKind::Execution));
        assert!(result.render().starts_with("FAILED: search blocked:"));
    }
}
