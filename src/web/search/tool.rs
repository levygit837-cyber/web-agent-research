//! Search tool seam: name, schema, argument parsing, and the [`Searcher`]
//! handle the research agent dispatches to. Fan-out lives in `fanout`.

use serde_json::{json, Value};

use super::ddg::DDG_HTML_URL;
use super::startpage::{STARTPAGE_HOME_URL, STARTPAGE_SEARCH_URL};
use crate::web::search::types::{Recency, SearchInput, SearchOutput, SearchProviderError};

/// Agent-loop tool name.
pub const SEARCH_TOOL_NAME: &str = "search";
/// One-line purpose for the system-prompt roster.
pub const SEARCH_TOOL_PURPOSE: &str =
    "Search the web; returns Hits (title, URL, snippet) to fetch, never cite directly.";

pub fn search_tool_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "name": SEARCH_TOOL_NAME,
        "description": "Fetch-only web search over DuckDuckGo and Startpage. Takes 1-8 queries, fans out to both providers in parallel, dedups by URL key and returns consensus-ranked merged results. Returned snippets are ungrounded candidates, not Evidence: fetch a result URL before citing it.",
        "parameters": {
            "type": "object",
            "properties": {
                "queries": {
                    "type": "array",
                    "description": "Research queries for this Turn. 1-8 items, each 1-500 chars.",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": { "type": "string", "minLength": 1, "maxLength": 500 }
                },
                "top_k": {
                    "type": "integer",
                    "description": "Max merged results to return. Defaults to 15.",
                    "minimum": 1,
                    "maximum": 30,
                    "default": 15
                },
                "recency": {
                    "type": "string",
                    "description": "Optional freshness window, mapped per provider (DDG df / Startpage with_date). Omit for no time filter.",
                    "enum": ["day", "week", "month", "year"]
                }
            },
            "required": ["queries"],
            "additionalProperties": false
        }
    })
}

/// Parse model-supplied tool arguments into a validated [`SearchInput`].
/// Errors are model-facing repair hints.
pub fn parse_search_args(args: &Value) -> Result<SearchInput, String> {
    let object = args
        .as_object()
        .ok_or_else(|| format!("Provide arguments as an object; got {args}."))?;
    if let Some(extra) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "queries" | "top_k" | "recency"))
    {
        return Err(format!(
            "Unknown argument `{extra}`; allowed: queries, top_k, recency."
        ));
    }
    let queries = match object.get("queries") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("Provide queries as strings; got {item}."))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => {
            return Err(format!(
                "Provide queries as an array of strings; got {other}."
            ))
        }
        None => return Err("Provide queries as an array of 1-8 strings; got missing.".to_owned()),
    };
    let top_k = match object.get("top_k") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| format!("Provide top_k as an integer 1-30; got {value}."))?
                as usize,
        ),
    };
    let recency = match object.get("recency") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<Recency>(value.clone())
                .map_err(|_| format!("Provide recency as day|week|month|year; got {value}."))?,
        ),
    };
    SearchInput::validate(queries, top_k, recency).map_err(|err| match err {
        SearchProviderError::EmptyQuery => "Provide at least one non-empty query.".to_owned(),
        SearchProviderError::TooManyQueries { got, max } => {
            format!("Provide at most {max} queries of at most 500 chars; got {got}.")
        }
        other => format!("invalid search input: {}", other.code()),
    })
}

/// Search handle: shared HTTP client plus provider endpoints. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Searcher {
    client: reqwest::Client,
    ddg: String,
    sp_home: String,
    sp_search: String,
    /// Engine suspension (#62) + pacing (#63) + allowlist (#64) state.
    /// [`super::governor::Governor::hermetic`] under `with_bases`/every
    /// test constructor: no persisted state, zero pacing delay, both
    /// providers always enabled (the hermetic rule).
    governor: super::governor::Governor,
    /// Disk cache root for search legs (`web::search::cache`); `None` under
    /// `with_bases` and every test constructor -- the hermetic rule: tests
    /// never touch the real user cache.
    cache_root: Option<std::path::PathBuf>,
}

impl Searcher {
    /// Production endpoints (DuckDuckGo HTML, Startpage), production
    /// `Governor` (persists to `<cache_root>/engines.json`) and disk cache
    /// (`web::search::cache`, under `<cache_root>/search/`), both resolved
    /// from `SEARCH_CACHE_DIR`/`XDG_CACHE_HOME`/`HOME`
    /// (`web::cache_dir::cache_root`).
    pub fn new() -> Self {
        let mut searcher = Self::with_bases(DDG_HTML_URL, STARTPAGE_HOME_URL, STARTPAGE_SEARCH_URL);
        let cache_root = crate::web::cache_dir::cache_root();
        searcher.governor = super::governor::Governor::new(cache_root.clone());
        searcher.cache_root = cache_root;
        searcher
    }

    /// Test seam: point every leg at local servers, hermetic `Governor`
    /// (no persisted state, no pacing delay, both providers enabled), no
    /// disk cache.
    #[doc(hidden)]
    pub fn with_bases(ddg: &str, sp_home: &str, sp_search: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            ddg: ddg.to_owned(),
            sp_home: sp_home.to_owned(),
            sp_search: sp_search.to_owned(),
            governor: super::governor::Governor::hermetic(),
            cache_root: None,
        }
    }

    /// Parallel multi-query fan-out; see `fanout::search_multi_with_bases`.
    pub async fn search(&self, input: SearchInput) -> Result<SearchOutput, SearchProviderError> {
        super::fanout::search_multi_with_bases(
            &self.client,
            input,
            &self.ddg,
            &self.sp_home,
            &self.sp_search,
            &self.governor,
            self.cache_root.as_deref(),
        )
        .await
    }
}

impl Default for Searcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::types::Recency;

    #[test]
    fn parse_search_args_accepts_schema_shape_and_rejects_the_rest() {
        let input =
            parse_search_args(&json!({"queries": ["a", " b "], "top_k": 5, "recency": "week"}))
                .expect("valid");
        assert_eq!(input.queries, vec!["a", "b"]);
        assert_eq!(input.top_k, 5);
        assert_eq!(input.recency, Some(Recency::Week));
        for bad in [
            json!({"query": "old shape"}),
            json!({"queries": []}),
            json!({"queries": "not an array"}),
            json!({"queries": [1]}),
            json!({"queries": ["a"], "recency": "fortnight"}),
            json!({"queries": ["a"], "top_k": -1}),
            json!("string"),
        ] {
            assert!(parse_search_args(&bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn tool_schema_shape_and_roundtrip() {
        let schema = search_tool_schema();
        assert_eq!(schema["name"], json!("search"));
        // `additionalProperties: false` everywhere — root, the
        // `parameters` object, and every `type: object` level below it. Only
        // `queries.items` is a non-object subschema and carries no flag.
        for pointer in ["", "/parameters"] {
            let node = schema.pointer(pointer).expect("schema node");
            assert_eq!(
                node.get("additionalProperties"),
                Some(&json!(false)),
                "missing flag at `{pointer}`"
            );
        }
        assert_eq!(schema["parameters"]["required"], json!(["queries"]));
        let queries = &schema["parameters"]["properties"]["queries"];
        assert_eq!(queries["minItems"], json!(1));
        assert_eq!(queries["maxItems"], json!(8));

        assert_eq!(queries["items"]["maxLength"], json!(500));
        let top_k = &schema["parameters"]["properties"]["top_k"];
        assert_eq!(top_k["minimum"], json!(1));
        assert_eq!(top_k["maximum"], json!(30));
        assert_eq!(top_k["default"], json!(15));
        let recency = &schema["parameters"]["properties"]["recency"];
        assert_eq!(recency["enum"], json!(["day", "week", "month", "year"]));
        // Valid payload validates ...
        assert!(SearchInput::validate(vec!["obscura browser".to_string()], Some(15), None).is_ok());
        assert!(
            SearchInput::validate(vec!["q".to_string()], Some(15), Some(Recency::Week),).is_ok()
        );
        // ... and invalid payloads fail: empty, too many, bad top_k clamps
        // (no error), over-long query, unknown recency string.
        assert_eq!(
            SearchInput::validate(vec![], None, None),
            Err(SearchProviderError::EmptyQuery)
        );
        assert!(SearchInput::validate(vec![" ".to_string()], None, None).is_err());
        let many: Vec<String> = (0..9).map(|i| format!("q{i}")).collect();
        assert_eq!(
            SearchInput::validate(many, None, None),
            Err(SearchProviderError::TooManyQueries { got: 9, max: 8 })
        );
        let long = "x".repeat(501);
        assert!(SearchInput::validate(vec![long], None, None).is_err());
        // 500 non-ASCII chars are 1000 bytes but legal: the bound counts
        // characters like the schema `maxLength`.
        assert!(SearchInput::validate(vec!["é".repeat(500)], None, None).is_ok());
        assert!(SearchInput::validate(vec!["é".repeat(501)], None, None).is_err());
        assert!(serde_json::from_str::<Recency>("\"fortnight\"").is_err());
        assert_eq!(Recency::Week.param(), "w");
        // Unknown properties are structurally rejected at every object level:
        // the schema declares no other keys beside the known ones.
        let known_root: Vec<&str> = schema
            .as_object()
            .expect("root object")
            .keys()
            .map(String::as_str)
            .collect();
        assert!(known_root.contains(&"$schema"));
        assert!(known_root.contains(&"name"));
        assert!(!known_root.contains(&"extra"));
        let known_params: Vec<&str> = schema["parameters"]
            .as_object()
            .expect("parameters object")
            .keys()
            .map(String::as_str)
            .collect();
        assert!(known_params.contains(&"properties"));
        assert!(!known_params.contains(&"extra"));
    }
}
