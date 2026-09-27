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
use crate::web::fetch::{Evidence, FetchError, Fetcher};
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
            Self::Search { hits } => {
                if hits.is_empty() {
                    return "no results".to_owned();
                }
                hits.iter()
                    .map(|hit| format!("- [{}]({})\n  {}", hit.title, hit.display_url, hit.snippet))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::Fetch { evidence } => {
                format!("{}\n\nSource: {}", evidence.markdown, evidence.source_url)
            }
            Self::AlreadyFetched { url, first_url } => format!(
                "ALREADY FETCHED: {url} was already fetched earlier in this Research (as {first_url}). Use that earlier content, or fetch a different Hit."
            ),
            #[cfg(test)]
            Self::Hang => "hanging".to_owned(),
            Self::Failed { reason, .. } => format!("FAILED: {reason}"),
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

    /// Execution worked (even an empty hit list, or a skipped repeat fetch,
    /// is information, not failure).
    pub fn is_success(&self) -> bool {
        !matches!(self, Self::Failed { .. })
    }

    /// `Some` naming which kind of failure this is; `None` for a success.
    pub fn failure_kind(&self) -> Option<FailureKind> {
        match self {
            Self::Failed { kind, .. } => Some(*kind),
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
            Searcher::with_bases(dead, dead, dead),
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
                    Ok(output) => ToolResult::Search {
                        hits: self.mark_already_fetched(output.results),
                    },
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
        E::AllFailed { failures } => format!("all engines failed: {failures}"),
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
        assert!(search.render().contains("[T](https://example.com/t)"));
        assert_eq!(search.url(), None);
        let fetch = ToolResult::Fetch {
            evidence: Evidence::new("https://example.com/p".to_owned(), "Body".to_owned()),
        };
        assert_eq!(fetch.url().as_deref(), Some("https://example.com/p"));
        assert!(fetch.render().ends_with("Source: https://example.com/p"));
        assert_eq!(ToolResult::Search { hits: vec![] }.render(), "no results");
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
}
