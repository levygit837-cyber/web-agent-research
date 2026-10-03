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
//! already known renders with an `(already fetched)` marker. Known limit:
//! this is a pointer, not a cache — if turn-history truncation already
//! dropped the first fetch's Evidence from the transcript, the model still
//! sees only the pointer to it, not the content itself.

use std::collections::HashMap;
#[cfg(test)]
use std::collections::VecDeque;
use std::sync::Mutex;

use crate::llm::{RequestedToolCall, ToolDef};
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
    Search {
        hits: Vec<MergedResult>,
    },
    /// The `search` call ran no leg on purpose (#81: `recency` set, no
    /// enabled engine applies it). Success, not Hits and not a failure:
    /// the note tells the model to search again without `recency`.
    SearchNote {
        note: String,
    },
    Fetch {
        evidence: Evidence,
    },
    /// A repeat `fetch` of a URL already fetched in this Research (#43):
    /// no network I/O ran. `url` is the requested URL for this call;
    /// `first_url` is the canonical `Evidence.source_url` from the earlier
    /// fetch. Success, not Evidence: `url()` is `None`.
    AlreadyFetched {
        url: String,
        first_url: String,
    },
    /// Every Startpage/DuckDuckGo leg of this `search` call was a bot-wall
    /// Challenge (#53), i.e. `SearchProviderError::AllFailed { all_challenged: true, .. }`:
    /// the request never reached a usable results page. Distinct from the
    /// generic `Failed` so the runner can remember it across the whole run
    /// (`RunReport::search_blocked`) even though it counts the same as an
    /// execution failure for the repair budget. `detail` carries the
    /// per-engine challenge messages (Omp `formatSearchProviderFailures`).
    SearchBlocked {
        detail: String,
    },
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
    /// rendering; the runner applies `max_evidence_chars` after.
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
            // Source first: the runner caps this text at
            // `max_evidence_chars`, and the model needs the final URL to cite.
            Self::Fetch { evidence } => {
                format!("Source: {}\n\n{}", evidence.source_url, evidence.markdown)
            }
            Self::AlreadyFetched { url, first_url } => format!(
                "ALREADY FETCHED: {url} was already fetched earlier in this Research (as {first_url}). Use that earlier content, or fetch a different Hit."
            ),
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
            Self::Fetch { evidence } => Some(evidence.source_url.clone()),
            _ => None,
        }
    }

    /// Which engine produced the fetched Evidence (#31); `None` for every
    /// non-`Fetch` result.
    pub fn fetch_path(&self) -> Option<FetchPath> {
        match self {
            Self::Fetch { evidence } => evidence.fetch_path,
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
    /// `dedup_key(url) -> canonical Evidence.source_url` for every URL
    /// successfully fetched so far this run.
    fetched: Mutex<HashMap<String, String>>,
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
    /// of matching on the rendered reason text.
    pub async fn execute(&self, call: &RequestedToolCall) -> ToolResult {
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
                let requested_url = match FetchInput::parse(&args) {
                    Ok(input) => input.url,
                    Err(err) => {
                        return failed(
                            call,
                            format!("invalid args for 'fetch': {err}"),
                            FailureKind::Dispatch,
                        )
                    }
                };
                let requested_key = dedup_key(&requested_url);
                if let Some(first_url) = self
                    .fetched
                    .lock()
                    .expect("fetched lock")
                    .get(&requested_key)
                    .cloned()
                {
                    return ToolResult::AlreadyFetched {
                        url: requested_url,
                        first_url,
                    };
                }
                match fetch_tool(&args, fetcher).await {
                    Ok(evidence) => {
                        let source_key = dedup_key(&evidence.source_url);
                        let mut fetched = self.fetched.lock().expect("fetched lock");
                        fetched.insert(requested_key, evidence.source_url.clone());
                        fetched.insert(source_key, evidence.source_url.clone());
                        drop(fetched);
                        ToolResult::Fetch { evidence }
                    }
                    Err(err @ FetchError::InvalidUrl { .. }) => failed(
                        call,
                        format!("invalid args for 'fetch': {err}"),
                        FailureKind::Dispatch,
                    ),
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
            registry.execute(&call("search", "{}")).await,
            ToolResult::Search { .. }
        ));
        assert!(reason(registry.execute(&call("search", "{}")).await).contains("exhausted"));
    }

    #[tokio::test]
    async fn dispatch_failures_are_classified_before_io() {
        let registry = ToolRegistry::offline();
        let unknown = registry.execute(&call("Search", "{}")).await;
        assert_eq!(kind(&unknown), FailureKind::Dispatch);
        assert!(reason(unknown).contains("unknown tool"));
        let bad_json = registry.execute(&call("search", "{bad")).await;
        assert_eq!(kind(&bad_json), FailureKind::Dispatch);
        assert!(reason(bad_json).contains("not valid JSON"));
        let bad_args = registry.execute(&call("search", r#"{"query": "x"}"#)).await;
        assert_eq!(kind(&bad_args), FailureKind::Dispatch);
        assert!(reason(bad_args).starts_with("invalid args for 'search'"));
        let bad_url = registry
            .execute(&call("fetch", r#"{"url": "ftp://x/y"}"#))
            .await;
        assert_eq!(kind(&bad_url), FailureKind::Dispatch);
        assert!(reason(bad_url).starts_with("invalid args for 'fetch'"));
    }

    #[tokio::test]
    async fn unreachable_endpoints_are_execution_failures() {
        let registry = ToolRegistry::offline();
        let search = registry
            .execute(&call("search", r#"{"queries": ["x"]}"#))
            .await;
        assert_eq!(kind(&search), FailureKind::Execution);
        assert!(reason(search).starts_with("search failed"));
        let fetch = registry
            .execute(&call("fetch", r#"{"url": "http://127.0.0.1:9/page"}"#))
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
            evidence: Evidence::new("https://example.com/p".to_owned(), "Body".to_owned()),
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
    /// so that URL must reach the model even when the page body is cut at
    /// `max_evidence_chars` (#72: it used to trail the body, and the cap cut
    /// it from 17 of 18 fetched pages in the before measurement).
    #[test]
    fn source_url_survives_the_evidence_cap() {
        let fetch = ToolResult::Fetch {
            evidence: Evidence::new("https://example.com/final".to_owned(), "x".repeat(10_000)),
        };
        let capped = crate::research::agent_loop::context::cap_evidence(&fetch.render(), 4_000);
        assert!(capped.contains("https://example.com/final"), "{capped}");
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
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": url }).to_string(),
            ))
            .await;
        let evidence_url = match first {
            ToolResult::Fetch { evidence } => evidence.source_url,
            other => panic!("expected Fetch, got {other:?}"),
        };
        assert_eq!(pages.hits("/"), 1);

        let second = registry
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": url }).to_string(),
            ))
            .await;
        match &second {
            ToolResult::AlreadyFetched {
                url: repeated,
                first_url,
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
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": first_url }).to_string(),
            ))
            .await;
        assert!(matches!(first, ToolResult::Fetch { .. }));
        assert_eq!(pages.hits("/"), 1);

        let second = registry
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": second_url }).to_string(),
            ))
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
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": from_url }).to_string(),
            ))
            .await;
        let final_url = match first {
            ToolResult::Fetch { evidence } => evidence.source_url,
            other => panic!("expected Fetch, got {other:?}"),
        };
        assert_eq!(
            final_url, to_url,
            "Evidence.source_url must be the post-redirect URL"
        );

        let second = registry
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": to_url }).to_string(),
            ))
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
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": url }).to_string(),
            ))
            .await;
        assert!(matches!(first, ToolResult::Failed { .. }));
        assert_eq!(pages.hits("/"), 1);

        let second = registry
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": url }).to_string(),
            ))
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
            .execute(&call(
                "fetch",
                &serde_json::json!({ "url": url_a }).to_string(),
            ))
            .await;
        assert!(matches!(fetched, ToolResult::Fetch { .. }));

        let searched = registry
            .execute(&call(
                "search",
                &serde_json::json!({ "queries": ["obscura"] }).to_string(),
            ))
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
            .execute(&call("search", r#"{"queries": ["q"]}"#))
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
