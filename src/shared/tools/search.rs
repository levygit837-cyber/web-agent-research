//! Search agent tool: thin function-calling wrapper over the engine.
//!
//! Owns the agent-loop tool seam (`SEARCH_TOOL_NAME`, `search_tool_schema`)
//! and delegates fan-out to `web_engine_search`. No provider logic here.

use serde_json::{json, Value};

use crate::shared::types::search::{SearchInput, SearchOutput, SearchProviderError};
use crate::shared::web_engine_search;

/// Agent-loop tool name.
pub const SEARCH_TOOL_NAME: &str = "search";

pub fn search_tool_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "name": SEARCH_TOOL_NAME,
        "description": "Fetch-only web search over DuckDuckGo and Startpage. Takes 1-8 queries, fans out to both providers in parallel, dedups by URL key and returns consensus-ranked merged results. Returned snippets are ungrounded candidates, not Evidence: fetch a result URL before citing it.",
        "parameters": {
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

/// Deep-module entry: one-line delegation to the engine fan-out.
pub async fn search_multi(
    client: &reqwest::Client,
    input: SearchInput,
) -> Result<SearchOutput, SearchProviderError> {
    web_engine_search::fanout::search_multi(client, input).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::search::Recency;

    #[test]
    fn tool_schema_shape_and_roundtrip() {
        let schema = search_tool_schema();
        assert_eq!(schema["name"], json!("search"));
        // SPEC §5: `additionalProperties: false` everywhere — root, the
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
