//! Fetch tool seam: schema + input parsing over [`Fetcher`].

use crate::web::fetch::error::FetchError;
use crate::web::fetch::fetcher::Fetcher;
use crate::web::fetch::Evidence;
use serde_json::Value;

/// Tool name registered with the agent loop.
pub const FETCH_TOOL_NAME: &str = "fetch";
/// One-line purpose for the system-prompt roster.
pub const FETCH_TOOL_PURPOSE: &str =
    "Read one page. Returns its final URL and its content as markdown: Evidence you can cite.";

/// Caller-supplied tool input: exactly what `fetch_tool_schema` advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchInput {
    pub url: String,
}

impl FetchInput {
    /// Shape check only: `value` must be an object with a string `url`.
    /// URL *validity* is the engine's job; parse errors surface as
    /// `FetchError::InvalidUrl { input }`.
    pub fn parse(value: &Value) -> Result<Self, FetchError> {
        let obj = match value.as_object() {
            Some(obj) => obj,
            None => {
                return Err(FetchError::InvalidUrl {
                    input: compact(value),
                });
            }
        };
        match obj.get("url").and_then(Value::as_str) {
            Some(url) => Ok(Self {
                url: url.to_string(),
            }),
            None => Err(FetchError::InvalidUrl {
                input: match obj.get("url") {
                    Some(v) => compact(v),
                    None => String::new(),
                },
            }),
        }
    }
}

/// The mandatory tool-definition object consumed by the agent loop.
/// Single source of truth: loop wiring and tests share this value.
pub fn fetch_tool_schema() -> Value {
    serde_json::json!({
        "name": FETCH_TOOL_NAME,
        "description": "Download one web page and return its final URL (after redirects) and the page converted to markdown, with scripts, styles, navigation and footer blocks, and forms removed. Images are reduced to their alt text (dropped entirely if they have none), data URIs are replaced with a short placeholder, and tracking parameters are stripped from link targets. This is the only way to get Evidence: a page counts as a source only after you fetch it, and citations must use the URL this tool returns. Use it on the most promising search Hits, on URLs given in the research goal, or on links inside a page you already fetched; never on a guessed URL. Pages that need JavaScript are rendered in a headless browser, which takes a few seconds longer. Long pages are cut, so the end of a very long reference page may be missing. If the page cannot be read (HTTP error, bot wall, timeout), the result starts with FAILED: pick a different Hit instead of retrying the same URL.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "format": "uri",
                    "description": "Absolute http(s) URL of the page to read, copied exactly from a search Hit, the research goal, or a link inside a page you already fetched."
                }
            },
            "required": ["url"],
            "additionalProperties": false
        }
    })
}

/// Validate input, fetch (static first, browser fallback), wrap in Evidence.
/// `Evidence.source_url` / `.markdown` come from `FetchedMarkdown`;
/// `collected_at` is stamped by `Evidence::new`; `fetch_path` records which
/// engine produced `markdown` (#31).
pub async fn fetch_tool(input: &Value, fetcher: &Fetcher) -> Result<Evidence, FetchError> {
    let parsed = FetchInput::parse(input)?;
    let (fetched, path) = fetcher.fetch(&parsed.url).await?;
    let mut evidence = Evidence::new(fetched.url, fetched.markdown);
    evidence.fetch_path = Some(path);
    Ok(evidence)
}

/// Compact JSON rendering for `InvalidUrl` payloads.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}
