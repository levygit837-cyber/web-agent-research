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

use super::fanout::{map_transport_error, parse_retry_after};
use crate::web::crates_io::{docs_url, is_crate_name, API_GATE};
use crate::web::search::decode::{collapse_whitespace, percent_encode};
use crate::web::search::markup::rows_or_drift;
use crate::web::search::types::{
    Recency, SearchProvider, SearchProviderError, SearchResult, MAX_NUM_RESULTS,
};
use crate::web::search::{apply_api_headers, leg_timeout};

/// Production search endpoint (`web::crates_io::API_BASE` + `/crates`).
/// Tests override via [`cratesio_search_with_base`].
pub const CRATESIO_SEARCH_URL: &str = "https://crates.io/api/v1/crates";

/// Rows asked of the API per request (page 1 only).
pub(crate) const CRATESIO_PER_PAGE: usize = 10;

/// Rows kept for a keyword route (`rust http client`): the vertical's
/// few best matches, so a broad term does not fill `top_k` with crates
/// that each carry the vertical leg weight.
pub(crate) const KEYWORD_ROWS: usize = 3;

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

/// The search request URL for the crate-ish `term`.
pub(crate) fn search_url(base: &str, term: &str) -> String {
    format!(
        "{base}?q={}&per_page={CRATESIO_PER_PAGE}",
        percent_encode(term)
    )
}

/// What the router sends to crates.io for one Query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    /// The `q` value.
    pub(crate) term: String,
    /// `true` when `term` names one crate (rules 1-4 of [`route`]): the
    /// leg keeps only that crate's row. `false` for a keyword route
    /// (rule 5): the first [`KEYWORD_ROWS`] rows.
    pub(crate) exact: bool,
}

/// Router: `Some(route)` when `query` is crate-shaped; `None` keeps the
/// leg out of the fan-out. Rules, in order (first match wins):
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
///
/// Rules 1-4 are exact-name routes, rule 5 a keyword route.
pub(crate) fn route(query: &str) -> Option<Route> {
    route_term(query).map(|(term, exact)| Route { term, exact })
}

fn route_term(query: &str) -> Option<(String, bool)> {
    for span in query.split('`').skip(1).step_by(2) {
        if is_crate_name(span) && !span.chars().any(|c| c.is_ascii_uppercase()) {
            return Some((span.to_string(), true));
        }
    }
    let tokens: Vec<&str> = query.split_whitespace().map(trim_token).collect();
    let mut saw_path = false;
    for token in &tokens {
        if let Some((root, _)) = token.split_once("::") {
            saw_path = true;
            if is_crate_name(root) && !BUILTIN_ROOTS.contains(&root) {
                return Some((root.to_string(), true));
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
                return Some((neighbor.to_string(), true));
            }
        }
    }
    if let [only] = tokens.as_slice() {
        if is_crate_name(only)
            && only.contains(['-', '_'])
            && !only.chars().any(|c| c.is_ascii_uppercase())
        {
            return Some(((*only).to_string(), true));
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
    (!rest.is_empty()).then(|| (rest.join(" "), false))
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

/// `true` when `row` is the crate `term` names: crates.io treats `-` and
/// `_` as the same character and names as case-insensitive.
fn names_crate(row: &CrateRow, term: &str) -> bool {
    let fold = |s: &str| s.to_ascii_lowercase().replace('_', "-");
    fold(&row.name) == fold(term)
}

/// The rows a routed leg keeps, in API order: an exact route keeps only
/// the named crate's row, else (the name is not on crates.io, or a
/// keyword route) the first [`KEYWORD_ROWS`]. Every kept row carries the
/// vertical's leg weight in the fusion, so the leg stays short.
fn keep_rows(rows: Vec<CrateRow>, route: &Route) -> Vec<CrateRow> {
    if route.exact {
        if let Some(named) = rows.iter().find(|row| names_crate(row, &route.term)) {
            return vec![named.clone()];
        }
    }
    rows.into_iter().take(KEYWORD_ROWS).collect()
}

/// Map a finished search exchange to Hits or a typed leg error. Any
/// non-2xx is `Upstream` with its real status and `Retry-After`: the API
/// is documented and keyless, so a 429 is its stated rate limit (the
/// governor suspends on it), not a bot wall. A 2xx body without a
/// `crates` array is the [`rows_or_drift`] typed drift error; an empty
/// array is a recognized "no results".
pub(crate) fn map_cratesio_response(
    status: u16,
    body: &str,
    query: &str,
    route: &Route,
    retry_after_secs: Option<u64>,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    if !(200..300).contains(&status) {
        return Err(SearchProviderError::Upstream {
            provider: SearchProvider::CratesIo,
            detail: format!("crates.io API error ({status})"),
            status: Some(status),
            retry_after_secs,
        });
    }
    let parsed = parse_search_json(body);
    let recognized = parsed.is_ok();
    let hits = keep_rows(parsed.unwrap_or_default(), route)
        .into_iter()
        .enumerate()
        .map(|(rank, row)| SearchResult {
            title: row.title(),
            url: row.docs_url(),
            snippet: row.description,
            provider: SearchProvider::CratesIo,
            query: query.to_string(),
            rank,
            published_date: None,
        })
        .collect();
    rows_or_drift(SearchProvider::CratesIo, status, body, hits, |_| recognized)
}

/// crates.io leg: one API GET for the routed term of `query`, through
/// the process-wide [`API_GATE`] (1 request/s, shared with the crate-page
/// fetch) with the honest [`apply_api_headers`] User-Agent. A Query the
/// router rejects has no crates.io answer: `Ok(vec![])` without a request
/// (the fan-out never schedules such a leg).
pub(crate) async fn cratesio_search(
    client: &reqwest::Client,
    query: &str,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    let Some(route) = route(query) else {
        return Ok(Vec::new());
    };
    let transport = |err: reqwest::Error, what: &str| {
        map_transport_error(
            SearchProvider::CratesIo,
            query,
            err.is_timeout(),
            format!("crates.io {what} failed: {err}"),
        )
    };
    API_GATE.wait().await;
    let response = apply_api_headers(
        client.get(search_url(base, &route.term)),
        "application/json",
    )
    .timeout(leg_timeout())
    .send()
    .await
    .map_err(|err| transport(err, "request"))?;
    let status = response.status().as_u16();
    let retry_after = parse_retry_after(&response);
    let body = response
        .text()
        .await
        .map_err(|err| transport(err, "body read"))?;
    map_cratesio_response(status, &body, query, &route, retry_after)
}

#[doc(hidden)]
pub async fn cratesio_search_with_base(
    client: &reqwest::Client,
    query: &str,
    _recency: Option<Recency>,
    base: &str,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    // The search API has no date filter; accepted for signature symmetry.
    cratesio_search(client, query, base).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CannedResponse, CannedServer};
    use crate::web::search::markup::UNRECOGNIZED_MARKUP;

    const SEARCH_BYTES: &str = include_str!("../../../tests/fixtures/cratesio/search-bytes.json");
    const SEARCH_EMPTY: &str = include_str!("../../../tests/fixtures/cratesio/search-empty.json");
    const BYTES_QUESTION: &str = "What does the `bytes` crate's `Buf` trait provide?";

    fn exact(term: &str) -> Route {
        Route {
            term: term.to_string(),
            exact: true,
        }
    }

    #[test]
    fn router_table() {
        // (Query, routed term, exact-name route)
        let cases: &[(&str, Option<(&str, bool)>)] = &[
            // Rule 1: backticked lowercase crate name.
            (BYTES_QUESTION, Some(("bytes", true))),
            ("how to use `serde_json::Value`", Some(("serde_json", true))),
            ("`HashMap` iteration order", None),
            // Rule 2: path root; builtin roots are not crates.
            ("tokio::sync::Mutex vs std Mutex", Some(("tokio", true))),
            ("std::sync::Mutex poisoning", None),
            ("std::collections::HashMap in rust", None),
            // Rule 3: the word next to `crate`.
            ("bytes crate Buf trait", Some(("bytes", true))),
            ("the reqwest crate timeout", Some(("reqwest", true))),
            ("crate anyhow context", Some(("anyhow", true))),
            ("Rust crate for parsing TOML", Some(("parsing TOML", false))),
            // Rule 4: one lowercase token with `-`/`_`.
            ("serde_json", Some(("serde_json", true))),
            ("tokio-util", Some(("tokio-util", true))),
            ("Tokio-Util", None),
            ("bytes", None),
            // Rule 5: keyword present -> the rest of the Query.
            ("rust http client", Some(("http client", false))),
            (
                "Rust async runtime comparison",
                Some(("async runtime comparison", false)),
            ),
            (
                "cargo workspace inheritance",
                Some(("workspace inheritance", false)),
            ),
            ("rust", None),
            ("rust crate", None),
            // Not crate-shaped.
            ("latest python release notes", None),
            ("weather in São Paulo", None),
            ("how-to guide", None),
            ("", None),
        ];
        for (query, want) in cases {
            let got = route(query);
            let got = got.as_ref().map(|r| (r.term.as_str(), r.exact));
            assert_eq!(got, *want, "query={query:?}");
        }
    }

    #[test]
    fn search_url_encodes_the_term() {
        assert_eq!(
            search_url(CRATESIO_SEARCH_URL, "http client"),
            "https://crates.io/api/v1/crates?q=http+client&per_page=10"
        );
        assert_eq!(
            CRATESIO_SEARCH_URL,
            format!("{}/crates", crate::web::crates_io::API_BASE)
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

    #[test]
    fn exact_route_maps_only_the_named_crate_to_a_docs_rs_hit() {
        let hits = map_cratesio_response(200, SEARCH_BYTES, BYTES_QUESTION, &exact("bytes"), None)
            .expect("hits");
        assert_eq!(hits.len(), 1, "{hits:?}");
        let hit = &hits[0];
        assert_eq!(hit.title, "bytes 1.12.1");
        assert_eq!(hit.url, "https://docs.rs/bytes");
        assert_eq!(hit.snippet, "Types and traits for working with bytes");
        assert_eq!(hit.provider, SearchProvider::CratesIo);
        assert_eq!(hit.query, BYTES_QUESTION);
        assert_eq!(hit.rank, 0);
    }

    #[test]
    fn exact_route_matches_names_across_dash_underscore_and_case() {
        let row = |name: &str| CrateRow {
            name: name.into(),
            version: "1.0.0".into(),
            description: String::new(),
        };
        assert!(names_crate(&row("tokio_util"), "tokio-util"));
        assert!(names_crate(&row("Inflector"), "inflector"));
        assert!(!names_crate(&row("bytes-utils"), "bytes"));
    }

    #[test]
    fn keyword_route_and_an_unknown_name_keep_the_first_rows() {
        let keyword = Route {
            term: "bytes buffers".into(),
            exact: false,
        };
        let hits = map_cratesio_response(200, SEARCH_BYTES, "rust bytes buffers", &keyword, None)
            .expect("hits");
        assert_eq!(hits.len(), KEYWORD_ROWS);
        assert_eq!(hits[1].url, "https://docs.rs/bitfields");
        assert_eq!(hits[1].rank, 1);
        let unknown =
            map_cratesio_response(200, SEARCH_BYTES, "q", &exact("nosuch"), None).expect("hits");
        assert_eq!(unknown.len(), KEYWORD_ROWS);
    }

    #[test]
    fn empty_array_is_no_results_and_other_bodies_are_typed_drift() {
        let empty = map_cratesio_response(200, SEARCH_EMPTY, "q", &exact("zzz"), None);
        assert_eq!(empty.expect("recognized empty answer"), vec![]);
        for body in ["<html>maintenance</html>", r#"{"errors":[]}"#] {
            match map_cratesio_response(200, body, "q", &exact("bytes"), None) {
                Err(SearchProviderError::Upstream {
                    provider: SearchProvider::CratesIo,
                    detail,
                    status: Some(200),
                    retry_after_secs: None,
                }) if detail == UNRECOGNIZED_MARKUP => {}
                other => panic!("{body}: expected drift, got {other:?}"),
            }
        }
    }

    #[test]
    fn rate_limit_is_upstream_with_its_retry_after() {
        match map_cratesio_response(429, "{}", "q", &exact("bytes"), Some(7)) {
            Err(SearchProviderError::Upstream {
                provider: SearchProvider::CratesIo,
                status: Some(429),
                retry_after_secs: Some(7),
                ..
            }) => {}
            other => panic!("expected Upstream 429, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn leg_sends_the_routed_term_with_the_api_user_agent() {
        let server =
            CannedServer::spawn(vec![CannedResponse::text(200, SEARCH_BYTES)
                .with_header("content-type", "application/json")]);
        let base = format!("{}/api/v1/crates", server.base());
        let client = reqwest::Client::new();
        let hits = cratesio_search_with_base(&client, BYTES_QUESTION, None, &base)
            .await
            .expect("hits");
        assert_eq!(hits[0].url, "https://docs.rs/bytes");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let request = requests[0].to_ascii_lowercase();
        assert!(
            request.starts_with("get /api/v1/crates?q=bytes&per_page=10 "),
            "{request}"
        );
        assert!(
            request.contains(&format!(
                "user-agent: {}",
                crate::web::api_user_agent().to_ascii_lowercase()
            )),
            "{request}"
        );
        assert!(request.contains("accept: application/json"), "{request}");
        assert!(!request.contains("sec-fetch"), "{request}");
    }

    #[tokio::test]
    async fn unrouted_query_sends_nothing() {
        let server = CannedServer::spawn(vec![]);
        let client = reqwest::Client::new();
        let hits = cratesio_search(&client, "weather in São Paulo", server.base())
            .await
            .expect("no leg");
        assert!(hits.is_empty());
        assert_eq!(server.hits(), 0);
    }
}
