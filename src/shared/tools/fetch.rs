//! Extract data from a URL (static fetch; browser fallback for JS).

use crate::shared::obscura::browser::{FetchError, Obscura};
use crate::shared::session::Evidence;
use serde_json::Value;

/// Tool name registered with the agent loop.
pub const FETCH_TOOL_NAME: &str = "fetch";

/// Caller-supplied tool input: exactly what the §6 schema advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchInput {
    pub url: String,
}

impl FetchInput {
    /// Shape check only: `value` must be an object with a string `url`.
    /// URL *validity* is the engine's job (§1); parse errors surface as
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

/// The mandatory tool-definition object consumed by the agent loop (#12).
/// Single source of truth: loop wiring and tests share this value.
pub fn fetch_tool_schema() -> Value {
    serde_json::json!({
        "name": "fetch",
        "description": "Fetch a URL via the Obscura engine and return its content as markdown Evidence for synthesis.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "format": "uri",
                    "description": "Absolute http(s) URL to fetch markdown from."
                }
            },
            "required": ["url"],
            "additionalProperties": false
        }
    })
}

/// Validate input, delegate to the engine, wrap markdown in Evidence.
/// `Evidence.source_url` / `.markdown` come from `FetchedMarkdown`;
/// `collected_at` is stamped by `Evidence::new`.
pub async fn fetch_tool(input: &Value, engine: &Obscura) -> Result<Evidence, FetchError> {
    let parsed = FetchInput::parse(input)?;
    let fetched = engine.fetch_markdown(&parsed.url).await?;
    Ok(Evidence::new(fetched.url, fetched.markdown))
}

/// Compact JSON rendering for `InvalidUrl` payloads.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}
