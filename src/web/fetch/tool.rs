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
    /// 1-based part of a long page; `None` means "not asked for", which the
    /// registry treats as part 1 for a new page and as a repeat fetch for a
    /// known one.
    pub part: Option<u32>,
}

impl FetchInput {
    /// Shape check only: `value` must be an object with a string `url` and,
    /// optionally, an integer `part >= 1`. URL *validity* is the engine's
    /// job; parse errors surface as `FetchError::InvalidUrl { input }`, a
    /// bad `part` as `FetchError::InvalidPart { input }`.
    pub fn parse(value: &Value) -> Result<Self, FetchError> {
        let obj = match value.as_object() {
            Some(obj) => obj,
            None => {
                return Err(FetchError::InvalidUrl {
                    input: compact(value),
                });
            }
        };
        let url = match obj.get("url").and_then(Value::as_str) {
            Some(url) => url.to_string(),
            None => {
                return Err(FetchError::InvalidUrl {
                    input: match obj.get("url") {
                        Some(v) => compact(v),
                        None => String::new(),
                    },
                });
            }
        };
        let part = match obj.get("part") {
            None | Some(Value::Null) => None,
            Some(raw) => match raw.as_u64().and_then(|n| u32::try_from(n).ok()) {
                Some(n) if n >= 1 => Some(n),
                _ => {
                    return Err(FetchError::InvalidPart {
                        input: compact(raw),
                    })
                }
            },
        };
        Ok(Self { url, part })
    }
}

/// The mandatory tool-definition object consumed by the agent loop.
/// Single source of truth: loop wiring and tests share this value.
pub fn fetch_tool_schema() -> Value {
    serde_json::json!({
        "name": FETCH_TOOL_NAME,
        "description": "Download one web page and return its final URL (after redirects) and the page converted to markdown, with scripts, styles, navigation and footer blocks, and forms removed; when the page has a main-content region (`main`, `article`) the sidebars around it are dropped too. Images are reduced to their alt text (dropped entirely if they have none), data URIs are replaced with a short placeholder, and tracking parameters are stripped from link targets. This is the only way to get Evidence: a page counts as a source only after you fetch it, and citations must use the URL this tool returns. Use it on the most promising search Hits, on URLs given in the research goal, or on links inside a page you already fetched; never on a guessed URL. Pages that need JavaScript are rendered in a headless browser, which takes a few seconds longer. Long pages come in parts, and the result says `Part k of N`; to read more of a page, fetch the same url with `part` set. Private, loopback and other non-public addresses are refused. If the page cannot be read (HTTP error, bot wall, timeout, refused address), the result starts with FAILED: pick a different Hit instead of retrying the same URL.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "format": "uri",
                    "description": "Absolute http(s) URL of the page to read, copied exactly from a search Hit, the research goal, or a link inside a page you already fetched."
                },
                "part": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Which part of a long page to read; omit for part 1. Use the same url and the next part number from a previous result."
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn part_is_optional_and_only_a_positive_integer() {
        let url = "https://example.com/p";
        assert_eq!(FetchInput::parse(&json!({"url": url})).unwrap().part, None);
        assert_eq!(
            FetchInput::parse(&json!({"url": url, "part": null}))
                .unwrap()
                .part,
            None
        );
        assert_eq!(
            FetchInput::parse(&json!({"url": url, "part": 3}))
                .unwrap()
                .part,
            Some(3)
        );
        for bad in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("2"),
            json!(4_294_967_296_u64),
        ] {
            let err = FetchInput::parse(&json!({"url": url, "part": bad}))
                .expect_err(&format!("part={bad} must be rejected"));
            assert!(
                matches!(err, FetchError::InvalidPart { .. }),
                "part={bad} gave {err:?}"
            );
        }
    }
}
