//! Concrete tool dispatch for the agent loop: one adapter per tool.
//!
//! Zero `trait`, zero `dyn` (ADR-0004 one-adapter rule). Tool names
//! (`"search"`, `"fetch"`) are stable registry keys so #13/#14 integrate at
//! #16 without rename. Until then, live mode validates args against the
//! local schemas below and returns canned stub results; the schemas mirror
//! the shapes #13/#14 will own (`query`/`top_k`, `url`/`max_chars`) but live
//! here so `shared/tools/` stays untouched on this branch. Tests use
//! [`ToolRegistry::stub`] with a canned queue. The runner applies the
//! per-call timeout around [`ToolRegistry::execute`] so stub and live paths
//! share one deadline.

use std::collections::VecDeque;

use crate::shared::gateway::{RequestedToolCall, ToolDef};

const SEARCH_TOOL_NAME: &str = "search";
const SEARCH_TOOL_DESCRIPTION: &str = "Search the web for pages matching a query.";
const FETCH_TOOL_NAME: &str = "fetch";
const FETCH_TOOL_DESCRIPTION: &str = "Fetch a URL and return its content as markdown.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPage {
    pub url: String,
    pub title: String,
    pub markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchArgs {
    query: String,
    top_k: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FetchArgs {
    url: String,
    max_chars: usize,
}

/// One executed tool call, rendered into the observation transcript.
#[derive(Debug, Clone)]
pub enum ToolResult {
    Search {
        hits: Vec<SearchHit>,
    },
    Fetch {
        page: FetchedPage,
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
                    .map(|hit| format!("- [{}]({})\n  {}", hit.title, hit.url, hit.snippet))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::Fetch { page } => {
                format!(
                    "# {}\n{}\n\nSource: {}",
                    page.title, page.markdown, page.url
                )
            }
            #[cfg(test)]
            Self::Hang => "hanging".to_owned(),
            Self::Failed { reason, .. } => format!("FAILED: {reason}"),
        }
    }

    /// Source URL carried by successful results, if any.
    pub fn url(&self) -> Option<String> {
        match self {
            Self::Search { hits } => hits.first().map(|hit| hit.url.clone()),
            Self::Fetch { page } => Some(page.url.clone()),
            #[cfg(test)]
            Self::Hang => None,
            Self::Failed { .. } => None,
        }
    }

    /// Execution worked (even an empty hit list is information, not failure).
    pub fn is_success(&self) -> bool {
        !matches!(self, Self::Failed { .. })
    }
}

enum RegistryMode {
    Live,
    #[allow(dead_code)]
    Stub {
        queue: tokio::sync::Mutex<VecDeque<ToolResult>>,
    },
}

/// Concrete dispatch table for the tools one run may offer.
pub struct ToolRegistry {
    mode: RegistryMode,
}

fn value_kind(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(flag) => format!("{flag} (boolean)"),
        serde_json::Value::Number(number) => format!("{number} (number)"),
        serde_json::Value::String(text) => format!("{text:?} (string)"),
        serde_json::Value::Array(_) => "an array".to_owned(),
        serde_json::Value::Object(_) => "an object".to_owned(),
    }
}

fn stub_search_def() -> ToolDef {
    ToolDef {
        name: SEARCH_TOOL_NAME.to_owned(),
        description: SEARCH_TOOL_DESCRIPTION.to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Web-search query; must be non-empty."
                },
                "top_k": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 5,
                    "description": "How many hits to return (1-10)."
                }
            },
            "required": ["query"]
        }),
    }
}

fn stub_fetch_def() -> ToolDef {
    ToolDef {
        name: FETCH_TOOL_NAME.to_owned(),
        description: FETCH_TOOL_DESCRIPTION.to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "http(s) URL to fetch."
                },
                "max_chars": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100000,
                    "default": 4000,
                    "description": "Maximum characters to return (1-100000)."
                }
            },
            "required": ["url"]
        }),
    }
}

fn validate_search(args: &serde_json::Value) -> Result<SearchArgs, String> {
    let object = args
        .as_object()
        .ok_or_else(|| format!("Provide arguments as an object; got {}.", value_kind(args)))?;
    let query = match object.get("query") {
        Some(serde_json::Value::String(text)) if !text.trim().is_empty() => text.clone(),
        Some(serde_json::Value::String(_)) => {
            return Err("Provide query as a non-empty string; got an empty string.".to_owned());
        }
        Some(other) => {
            return Err(format!(
                "Provide query as a non-empty string; got {}.",
                value_kind(other)
            ));
        }
        None => return Err("Provide query as a non-empty string; got missing.".to_owned()),
    };
    let top_k = match object.get("top_k") {
        None => 5,
        Some(serde_json::Value::Number(number)) => match number.as_u64() {
            Some(value) if (1..=10).contains(&value) => value as u8,
            Some(value) => {
                return Err(format!("Provide top_k as an integer 1..=10; got {value}."));
            }
            None => {
                return Err(format!(
                    "Provide top_k as an integer 1..=10; got {number} (non-integer)."
                ));
            }
        },
        Some(other) => {
            return Err(format!(
                "Provide top_k as an integer 1..=10; got {}.",
                value_kind(other)
            ));
        }
    };
    Ok(SearchArgs { query, top_k })
}

fn validate_fetch(args: &serde_json::Value) -> Result<FetchArgs, String> {
    let object = args
        .as_object()
        .ok_or_else(|| format!("Provide arguments as an object; got {}.", value_kind(args)))?;
    let url = match object.get("url") {
        Some(serde_json::Value::String(text))
            if text.starts_with("http://") || text.starts_with("https://") =>
        {
            text.clone()
        }
        Some(serde_json::Value::String(_)) => {
            return Err(
                "Provide url as an http(s) URL; got a string with another scheme.".to_owned(),
            );
        }
        Some(other) => {
            return Err(format!(
                "Provide url as an http(s) URL; got {}.",
                value_kind(other)
            ));
        }
        None => return Err("Provide url as an http(s) URL; got missing.".to_owned()),
    };
    let max_chars = match object.get("max_chars") {
        None => 4_000,
        Some(serde_json::Value::Number(number)) => match number.as_u64() {
            Some(value) if (1..=100_000).contains(&value) => value as usize,
            Some(value) => {
                return Err(format!(
                    "Provide max_chars as a positive integer <= 100000; got {value}."
                ));
            }
            None => {
                return Err(format!(
                    "Provide max_chars as a positive integer <= 100000; got {number} (non-integer)."
                ));
            }
        },
        Some(other) => {
            return Err(format!(
                "Provide max_chars as a positive integer <= 100000; got {}.",
                value_kind(other)
            ));
        }
    };
    Ok(FetchArgs { url, max_chars })
}

async fn stub_execute_search(_args: SearchArgs) -> ToolResult {
    ToolResult::Search {
        hits: vec![SearchHit {
            title: "Obscura engine overview".to_owned(),
            url: "https://example.com/obscura".to_owned(),
            snippet: "Stub search hit; the real engine lands in #13.".to_owned(),
        }],
    }
}

async fn stub_execute_fetch(_args: FetchArgs) -> ToolResult {
    ToolResult::Fetch {
        page: FetchedPage {
            url: "https://example.com/obscura".to_owned(),
            title: "Obscura engine overview".to_owned(),
            markdown: "Stub fetch body; the real Obscura integration lands in #14.".to_owned(),
        },
    }
}

impl ToolRegistry {
    /// Live dispatch: validate args against the local schemas and return
    /// canned stub results until #13/#14 own the real bodies.
    pub fn live() -> Self {
        Self {
            mode: RegistryMode::Live,
        }
    }

    /// Internal test seam: pop canned results in wire order.
    #[allow(dead_code)]
    pub(crate) fn stub(results: VecDeque<ToolResult>) -> Self {
        Self {
            mode: RegistryMode::Stub {
                queue: tokio::sync::Mutex::new(results),
            },
        }
    }

    /// Tools known to this registry, in deterministic wire order.
    pub fn tool_names(&self) -> Vec<&'static str> {
        vec![SEARCH_TOOL_NAME, FETCH_TOOL_NAME]
    }

    /// One-line purposes aligned with the sysprompt roster.
    pub fn tool_purposes(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            (SEARCH_TOOL_NAME, SEARCH_TOOL_DESCRIPTION),
            (FETCH_TOOL_NAME, FETCH_TOOL_DESCRIPTION),
        ]
    }

    /// Wire schemas filtered by allowed names, in registry order.
    /// Unknown names are skipped silently: the runner's pre-flight already
    /// rejected them, and double-reporting would confuse the model.
    pub fn tool_defs(&self, allowed: &[String]) -> Vec<ToolDef> {
        let mut defs = Vec::new();
        for name in self.tool_names() {
            if allowed.iter().any(|allowed| allowed == name) {
                defs.push(match name {
                    SEARCH_TOOL_NAME => stub_search_def(),
                    FETCH_TOOL_NAME => stub_fetch_def(),
                    _ => continue,
                });
            }
        }
        defs
    }

    fn is_known(name: &str) -> bool {
        name == SEARCH_TOOL_NAME || name == FETCH_TOOL_NAME
    }

    /// Parse args + validate + run one call. Never panics: unknown names,
    /// bad JSON, and validator rejections all become [`ToolResult::Failed`].
    /// Dispatch failures (unknown name, bad JSON, invalid args) are
    /// validation-kind; executor failures are execution-kind — the runner
    /// distinguishes them from the rendered text.
    pub async fn execute(&self, call: &RequestedToolCall) -> ToolResult {
        if let RegistryMode::Stub { queue } = &self.mode {
            let next = queue
                .lock()
                .await
                .pop_front()
                .unwrap_or_else(|| ToolResult::Failed {
                    tool: call.name.clone(),
                    reason: "stub queue exhausted".to_owned(),
                });
            #[cfg(test)]
            if matches!(next, ToolResult::Hang) {
                std::future::pending::<()>().await;
                unreachable!("stub Hang pends until the runner timeout fires");
            }
            return next;
        }
        if !Self::is_known(&call.name) {
            return ToolResult::Failed {
                tool: call.name.clone(),
                reason: format!("unknown tool '{}'", call.name),
            };
        }
        let parsed = call.parsed_arguments();
        let args = match parsed {
            Ok(args) => args,
            Err(err) => {
                return ToolResult::Failed {
                    tool: call.name.clone(),
                    reason: format!("arguments are not valid JSON: {err}"),
                };
            }
        };
        match call.name.as_str() {
            SEARCH_TOOL_NAME => match validate_search(&args) {
                Ok(valid) => stub_execute_search(valid).await,
                Err(reason) => ToolResult::Failed {
                    tool: call.name.clone(),
                    reason: format!("invalid args for 'search': {reason}"),
                },
            },
            FETCH_TOOL_NAME => match validate_fetch(&args) {
                Ok(valid) => stub_execute_fetch(valid).await,
                Err(reason) => ToolResult::Failed {
                    tool: call.name.clone(),
                    reason: format!("invalid args for 'fetch': {reason}"),
                },
            },
            _ => ToolResult::Failed {
                tool: call.name.clone(),
                reason: format!("unknown tool '{}'", call.name),
            },
        }
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

    #[test]
    fn registry_names_are_deterministic() {
        let registry = ToolRegistry::live();
        assert_eq!(registry.tool_names(), vec!["search", "fetch"]);
    }

    #[test]
    fn tool_defs_filter_in_registry_order() {
        let registry = ToolRegistry::live();
        let defs = registry.tool_defs(&["fetch".to_owned(), "search".to_owned()]);
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "search");
        assert_eq!(defs[1].name, "fetch");
        for def in &defs {
            assert_eq!(
                def.parameters.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "each ToolDef must carry a real JSON Schema"
            );
        }
        let only_search = registry.tool_defs(&["search".to_owned()]);
        assert_eq!(only_search.len(), 1);
        assert_eq!(only_search[0].name, "search");
        assert!(registry.tool_defs(&["nope".to_owned()]).is_empty());
        assert!(registry.tool_defs(&[]).is_empty());
    }

    #[test]
    fn tool_defs_carry_required_fields() {
        let registry = ToolRegistry::live();
        let defs = registry.tool_defs(&["search".to_owned(), "fetch".to_owned()]);
        let search = defs
            .iter()
            .find(|def| def.name == "search")
            .expect("search def");
        let required = search
            .parameters
            .get("required")
            .and_then(|v| v.as_array())
            .expect("search schema must list required fields");
        assert!(required.contains(&serde_json::json!("query")));
        let fetch = defs
            .iter()
            .find(|def| def.name == "fetch")
            .expect("fetch def");
        let required = fetch
            .parameters
            .get("required")
            .and_then(|v| v.as_array())
            .expect("fetch schema must list required fields");
        assert!(required.contains(&serde_json::json!("url")));
    }

    #[tokio::test]
    async fn stub_pops_in_order_then_exhausts() {
        let registry = ToolRegistry::stub(VecDeque::from([
            ToolResult::Search { hits: vec![] },
            ToolResult::Failed {
                tool: "fetch".to_owned(),
                reason: "boom".to_owned(),
            },
        ]));
        let first = registry.execute(&call("search", "{}")).await;
        assert!(matches!(first, ToolResult::Search { .. }));
        let second = registry.execute(&call("fetch", "{}")).await;
        assert!(matches!(second, ToolResult::Failed { .. }));
        let third = registry.execute(&call("search", "{}")).await;
        match third {
            ToolResult::Failed { reason, .. } => {
                assert!(reason.contains("stub queue exhausted"), "got {reason}");
            }
            other => panic!("exhausted stub must fail deterministically, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_name_is_failed_not_panic() {
        let registry = ToolRegistry::live();
        let result = registry.execute(&call("Search", "{}")).await;
        match result {
            ToolResult::Failed { tool, reason } => {
                assert_eq!(tool, "Search");
                assert!(reason.contains("unknown tool"), "got {reason}");
            }
            other => panic!("unknown name must fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_json_args_are_failed() {
        let registry = ToolRegistry::live();
        let result = registry.execute(&call("search", "{bad json")).await;
        match result {
            ToolResult::Failed { reason, .. } => {
                assert!(reason.contains("not valid JSON"), "got {reason}");
            }
            other => panic!("bad JSON must fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validator_rejection_is_failed() {
        let registry = ToolRegistry::live();
        let result = registry.execute(&call("search", r#"{"query": ""}"#)).await;
        match result {
            ToolResult::Failed { reason, .. } => {
                assert!(reason.contains("invalid args for 'search'"), "got {reason}");
            }
            other => panic!("invalid args must fail, got {other:?}"),
        }
        let fetch_result = registry
            .execute(&call("fetch", r#"{"url": "ftp://x/y"}"#))
            .await;
        match fetch_result {
            ToolResult::Failed { reason, .. } => {
                assert!(reason.contains("invalid args for 'fetch'"), "got {reason}");
            }
            other => panic!("invalid fetch args must fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn live_valid_args_return_canned_results() {
        let registry = ToolRegistry::live();
        let search = registry
            .execute(&call("search", r#"{"query": "obscura"}"#))
            .await;
        assert!(matches!(search, ToolResult::Search { .. }));
        let fetch = registry
            .execute(&call("fetch", r#"{"url": "https://example.com/a"}"#))
            .await;
        assert!(matches!(fetch, ToolResult::Fetch { .. }));
    }

    #[test]
    fn render_shapes() {
        let empty = ToolResult::Search { hits: vec![] };
        assert_eq!(empty.render(), "no results");
        assert!(empty.is_success());
        let failed = ToolResult::Failed {
            tool: "search".to_owned(),
            reason: "boom".to_owned(),
        };
        assert_eq!(failed.render(), "FAILED: boom");
        assert!(!failed.is_success());
        assert_eq!(failed.url(), None);
        let hit = ToolResult::Search {
            hits: vec![SearchHit {
                title: "T".to_owned(),
                url: "https://example.com/t".to_owned(),
                snippet: "S".to_owned(),
            }],
        };
        assert!(hit.render().contains("https://example.com/t"));
        assert_eq!(hit.url().as_deref(), Some("https://example.com/t"));
    }
}
