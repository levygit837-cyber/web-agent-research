//! crates.io keyless vertical (#109): the JSON search API as one more
//! engine leg, joined only for crate-shaped Queries.
//!
//! The data-access policy (1 API request/s, identifying User-Agent) lives
//! in `web::crates_io`. The leg sends the crate-ish term the router
//! extracts from the Query, not the whole question: crates.io's full-text
//! ranking puts an exact crate name first (`exact_match`), but a
//! multi-word question buries it (measured 2026-10-05: `q=bytes` ranks
//! `bytes` first, `q=bytes buf` does not return it in the top 5).
//!
//! Hits point at the crate's docs.rs page (`web::crates_io::docs_url`),
//! never at `crates.io/crates/<name>`, which is a client-rendered shell.

use serde::Deserialize;

use crate::web::crates_io::{docs_url, is_crate_name, API_BASE};
use crate::web::search::decode::{collapse_whitespace, percent_encode};
use crate::web::search::types::MAX_NUM_RESULTS;

/// Rows asked of the API per request (page 1 only).
pub(crate) const CRATESIO_PER_PAGE: usize = 10;

/// Words that mark a Query as being about Rust packages. Compared against
/// lowercase tokens with edge punctuation and a trailing `'s` removed.
const CRATE_KEYWORDS: &[&str] = &[
    "crate",
    "crates",
    "cargo",
    "rust",
    "rustc",
    "rustup",
    "crates.io",
    "docs.rs",
    "msrv",
];

/// Path roots that are not crates.io packages (`std::sync::Mutex`).
const BUILTIN_ROOTS: &[&str] = &["std", "core", "alloc", "self", "super", "crate"];

/// Production search endpoint (`{API_BASE}/crates`); tests pass a stub
/// base to [`search_url`] instead.
pub(crate) fn default_search_base() -> String {
    format!("{API_BASE}/crates")
}

/// The search request URL for the crate-ish `term`.
pub(crate) fn search_url(base: &str, term: &str) -> String {
    format!(
        "{base}?q={}&per_page={CRATESIO_PER_PAGE}",
        percent_encode(term)
    )
}

/// Router: `Some(term)` when `query` is crate-shaped, with the term to
/// send to crates.io; `None` keeps the leg out of the fan-out. Rules, in
/// order (first match wins):
///
/// 1. A backticked span that is a lowercase crate name (`` `bytes` ``;
///    `` `HashMap` `` is a type, not a crate).
/// 2. The root of a Rust path (`tokio::sync::Mutex` -> `tokio`), unless it
///    is `std`/`core`/`alloc`/`self`/`super`/`crate`.
/// 3. The word next to `crate`/`crates` (`the bytes crate`, `crate bytes`).
/// 4. A single-token Query that is lowercase and holds `-` or `_`
///    (`serde_json`, `tokio-util`).
/// 5. Any other Query with a [`CRATE_KEYWORDS`] word: the Query minus those
///    words (`rust http client` -> `http client`); `None` if nothing is
///    left, or if a Rust path was present (rule 2 rejected a builtin root).
pub(crate) fn route(query: &str) -> Option<String> {
    for span in query.split('`').skip(1).step_by(2) {
        if is_crate_name(span) && !span.chars().any(|c| c.is_ascii_uppercase()) {
            return Some(span.to_string());
        }
    }
    let tokens: Vec<&str> = query.split_whitespace().map(trim_token).collect();
    let mut saw_path = false;
    for token in &tokens {
        if let Some((root, _)) = token.split_once("::") {
            saw_path = true;
            if is_crate_name(root) && !BUILTIN_ROOTS.contains(&root) {
                return Some(root.to_string());
            }
        }
    }
    let lower: Vec<String> = tokens.iter().map(|t| keyword_form(t)).collect();
    for (i, word) in lower.iter().enumerate() {
        if word != "crate" && word != "crates" {
            continue;
        }
        let before = i.checked_sub(1).map(|j| tokens[j]);
        let after = tokens.get(i + 1).copied();
        for neighbor in [before, after].into_iter().flatten() {
            if is_crate_name(neighbor)
                && !is_keyword(&keyword_form(neighbor))
                && !is_filler(neighbor)
            {
                return Some(neighbor.to_string());
            }
        }
    }
    if let [only] = tokens.as_slice() {
        if is_crate_name(only)
            && only.contains(['-', '_'])
            && !only.chars().any(|c| c.is_ascii_uppercase())
        {
            return Some((*only).to_string());
        }
    }
    if saw_path || !lower.iter().any(|w| is_keyword(w)) {
        return None;
    }
    let rest: Vec<&str> = tokens
        .iter()
        .zip(&lower)
        .filter(|(token, word)| !token.is_empty() && !is_keyword(word) && !is_filler(token))
        .map(|(token, _)| *token)
        .collect();
    (!rest.is_empty()).then(|| rest.join(" "))
}

/// Token without the punctuation a question wraps words in.
fn trim_token(token: &str) -> &str {
    token.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':'))
}

/// Lowercase token with a trailing possessive `'s` removed (`crate's`).
fn keyword_form(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    match lower.strip_suffix("'s") {
        Some(stem) => stem.to_string(),
        None => lower,
    }
}

fn is_keyword(word: &str) -> bool {
    CRATE_KEYWORDS.contains(&word)
}

/// Articles and pronouns that sit next to `crate` without naming one.
fn is_filler(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "the"
            | "this"
            | "that"
            | "which"
            | "what"
            | "best"
            | "my"
            | "your"
            | "for"
            | "of"
            | "to"
            | "in"
            | "on"
            | "is"
    )
}

/// One crate from a search response, reduced to what a Hit needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrateRow {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) description: String,
}

impl CrateRow {
    /// Hit title: `<name> <default_version>`.
    pub(crate) fn title(&self) -> String {
        format!("{} {}", self.name, self.version)
    }

    /// Hit URL: the crate's docs.rs page.
    pub(crate) fn docs_url(&self) -> String {
        docs_url(&self.name)
    }
}

#[derive(Deserialize)]
struct SearchBody {
    crates: Vec<ApiCrate>,
}

#[derive(Deserialize)]
struct ApiCrate {
    name: String,
    #[serde(default)]
    default_version: Option<String>,
    #[serde(default)]
    max_version: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// Search response body -> rows, at most [`MAX_NUM_RESULTS`], in API
/// order. `Err` means the body is not the documented shape (no `crates`
/// array): markup drift, never "zero results". An empty `crates` array is
/// `Ok(vec![])`. A crate whose name is not a valid crate name is skipped;
/// one with no `default_version` (every version yanked) falls back to
/// `max_version`, else is skipped.
pub(crate) fn parse_search_json(body: &str) -> Result<Vec<CrateRow>, String> {
    let parsed: SearchBody = serde_json::from_str(body)
        .map_err(|err| format!("crates.io search body is not the documented shape: {err}"))?;
    Ok(parsed
        .crates
        .into_iter()
        .filter(|c| is_crate_name(&c.name))
        .filter_map(|c| {
            let version = c.default_version.or(c.max_version)?;
            Some(CrateRow {
                name: c.name,
                version,
                description: collapse_whitespace(c.description.as_deref().unwrap_or("")),
            })
        })
        .take(MAX_NUM_RESULTS)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_BYTES: &str = include_str!("../../../tests/fixtures/cratesio/search-bytes.json");
    const SEARCH_EMPTY: &str = include_str!("../../../tests/fixtures/cratesio/search-empty.json");

    #[test]
    fn router_table() {
        let cases: &[(&str, Option<&str>)] = &[
            // Rule 1: backticked lowercase crate name.
            (
                "What does the `bytes` crate's `Buf` trait provide?",
                Some("bytes"),
            ),
            ("how to use `serde_json::Value`", Some("serde_json")),
            ("`HashMap` iteration order", None),
            // Rule 2: path root; builtin roots are not crates.
            ("tokio::sync::Mutex vs std Mutex", Some("tokio")),
            ("std::sync::Mutex poisoning", None),
            ("std::collections::HashMap in rust", None),
            // Rule 3: the word next to `crate`.
            ("bytes crate Buf trait", Some("bytes")),
            ("the reqwest crate timeout", Some("reqwest")),
            ("crate anyhow context", Some("anyhow")),
            ("Rust crate for parsing TOML", Some("parsing TOML")),
            // Rule 4: one lowercase token with `-`/`_`.
            ("serde_json", Some("serde_json")),
            ("tokio-util", Some("tokio-util")),
            ("Tokio-Util", None),
            ("bytes", None),
            // Rule 5: keyword present -> the rest of the Query.
            ("rust http client", Some("http client")),
            (
                "Rust async runtime comparison",
                Some("async runtime comparison"),
            ),
            ("cargo workspace inheritance", Some("workspace inheritance")),
            ("rust", None),
            ("rust crate", None),
            // Not crate-shaped.
            ("latest python release notes", None),
            ("weather in São Paulo", None),
            ("how-to guide", None),
            ("", None),
        ];
        for (query, want) in cases {
            assert_eq!(route(query).as_deref(), *want, "query={query:?}");
        }
    }

    #[test]
    fn search_url_encodes_the_term() {
        assert_eq!(
            search_url(&default_search_base(), "http client"),
            "https://crates.io/api/v1/crates?q=http+client&per_page=10"
        );
    }

    #[test]
    fn parses_fixture_rows_in_api_order() {
        let rows = parse_search_json(SEARCH_BYTES).expect("fixture parses");
        assert_eq!(rows.len(), 10);
        let first = &rows[0];
        assert_eq!(first.name, "bytes");
        assert_eq!(first.title(), "bytes 1.12.1");
        assert_eq!(first.docs_url(), "https://docs.rs/bytes");
        assert_eq!(first.description, "Types and traits for working with bytes");
        assert_eq!(rows[1].name, "bitfields");
    }

    #[test]
    fn empty_result_is_ok_and_drift_is_err() {
        assert_eq!(parse_search_json(SEARCH_EMPTY), Ok(vec![]));
        assert!(parse_search_json("<html>maintenance</html>").is_err());
        assert!(parse_search_json(r#"{"errors":[{"detail":"x"}]}"#).is_err());
    }

    #[test]
    fn version_falls_back_and_bad_names_are_skipped() {
        let body = r#"{"crates":[
            {"name":"yanked-all","default_version":null,"max_version":"0.1.0","description":" a\n b "},
            {"name":"no-version","default_version":null,"description":null},
            {"name":"../evil","default_version":"1.0.0","description":""}
        ],"meta":{"total":3}}"#;
        let rows = parse_search_json(body).expect("parses");
        assert_eq!(
            rows,
            vec![CrateRow {
                name: "yanked-all".into(),
                version: "0.1.0".into(),
                description: "a b".into(),
            }]
        );
    }
}
