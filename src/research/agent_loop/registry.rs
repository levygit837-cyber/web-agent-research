//! Tool dispatch for the agent loop: `search` → [`Searcher`], `fetch` → [`Fetcher`].
//!
//! Schemas, argument parsing, and result types come from `web/` (ADR-0006 §4);
//! this module only routes calls and renders results for the model. Zero
//! `trait`, zero `dyn`. Tests swap the live tools for a `#[cfg(test)]` queue.
//! The runner applies the per-call timeout around [`ToolRegistry::execute`].

#[cfg(test)]
use std::collections::VecDeque;

use crate::llm::{RequestedToolCall, ToolDef};
use crate::web::fetch::tool::{fetch_tool, fetch_tool_schema, FETCH_TOOL_NAME, FETCH_TOOL_PURPOSE};
use crate::web::fetch::{Evidence, FetchError, Fetcher};
use crate::web::search::tool::{
    parse_search_args, search_tool_schema, Searcher, SEARCH_TOOL_NAME, SEARCH_TOOL_PURPOSE,
};
use crate::web::search::types::MergedResult;

/// One executed tool call, rendered into the observation transcript.
#[derive(Debug, Clone)]
pub enum ToolResult {
    /// Hits are candidates: rendered for the model, never Evidence.
    Search {
        hits: Vec<MergedResult>,
    },
    Fetch {
        evidence: Evidence,
    },
    #[cfg(test)]
    Hang,
    Failed {
        tool: String,
        reason: String,
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
            #[cfg(test)]
            Self::Hang => "hanging".to_owned(),
            Self::Failed { reason, .. } => format!("FAILED: {reason}"),
        }
    }

    /// Source URL of fetched Evidence. Search Hits carry none: they are not Evidence.
    pub fn url(&self) -> Option<String> {
        match self {
            Self::Fetch { evidence } => Some(evidence.source_url.clone()),
            _ => None,
        }
    }

    /// Execution worked (even an empty hit list is information, not failure).
    pub fn is_success(&self) -> bool {
        !matches!(self, Self::Failed { .. })
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

/// Dispatch table for the tools one run may offer.
pub struct ToolRegistry {
    backend: Backend,
}

impl ToolRegistry {
    /// Live dispatch over the real web tools.
    pub fn new(searcher: Searcher, fetcher: Fetcher) -> Self {
        Self {
            backend: Backend::Live { searcher, fetcher },
        }
    }

    /// Test seam: pop canned results in wire order, ignoring arguments.
    #[cfg(test)]
    pub(crate) fn stub(results: VecDeque<ToolResult>) -> Self {
        Self {
            backend: Backend::Queue(tokio::sync::Mutex::new(results)),
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
    /// bad JSON, and validator rejections all become [`ToolResult::Failed`].
    /// Dispatch failures (unknown name, bad JSON, invalid args) are
    /// validation-kind; executor failures are execution-kind — the runner
    /// distinguishes them from the rendered text.
    pub async fn execute(&self, call: &RequestedToolCall) -> ToolResult {
        let (searcher, fetcher) = match &self.backend {
            Backend::Live { searcher, fetcher } => (searcher, fetcher),
            #[cfg(test)]
            Backend::Queue(queue) => {
                let next = queue
                    .lock()
                    .await
                    .pop_front()
                    .unwrap_or_else(|| failed(call, "stub queue exhausted".to_owned()));
                if matches!(next, ToolResult::Hang) {
                    std::future::pending::<()>().await;
                }
                return next;
            }
        };
        let args = match call.parsed_arguments() {
            Ok(args) => args,
            Err(err) => return failed(call, format!("arguments are not valid JSON: {err}")),
        };
        match call.name.as_str() {
            SEARCH_TOOL_NAME => {
                let input = match parse_search_args(&args) {
                    Ok(input) => input,
                    Err(reason) => {
                        return failed(call, format!("invalid args for 'search': {reason}"))
                    }
                };
                match searcher.search(input).await {
                    Ok(output) => ToolResult::Search {
                        hits: output.results,
                    },
                    Err(err) => failed(call, format!("search failed: {}", search_error(&err))),
                }
            }
            FETCH_TOOL_NAME => match fetch_tool(&args, fetcher).await {
                Ok(evidence) => ToolResult::Fetch { evidence },
                Err(err @ FetchError::InvalidUrl { .. }) => {
                    failed(call, format!("invalid args for 'fetch': {err}"))
                }
                Err(err) => failed(call, format!("fetch failed: {err}")),
            },
            _ => failed(call, format!("unknown tool '{}'", call.name)),
        }
    }
}

fn failed(call: &RequestedToolCall, reason: String) -> ToolResult {
    ToolResult::Failed {
        tool: call.name.clone(),
        reason,
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
        assert!(reason(registry.execute(&call("Search", "{}")).await).contains("unknown tool"));
        assert!(reason(registry.execute(&call("search", "{bad")).await).contains("not valid JSON"));
        assert!(
            reason(registry.execute(&call("search", r#"{"query": "x"}"#)).await)
                .starts_with("invalid args for 'search'")
        );
        assert!(reason(
            registry
                .execute(&call("fetch", r#"{"url": "ftp://x/y"}"#))
                .await
        )
        .starts_with("invalid args for 'fetch'"));
    }

    #[tokio::test]
    async fn unreachable_endpoints_are_execution_failures() {
        let registry = ToolRegistry::offline();
        let search = reason(
            registry
                .execute(&call("search", r#"{"queries": ["x"]}"#))
                .await,
        );
        assert!(search.starts_with("search failed"), "{search}");
        let fetch = reason(
            registry
                .execute(&call("fetch", r#"{"url": "http://127.0.0.1:9/page"}"#))
                .await,
        );
        assert!(fetch.starts_with("fetch failed"), "{fetch}");
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
}
