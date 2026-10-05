//! Markup drift detection shared by every engine leg (#104).
//!
//! A 2xx page that yields zero rows is only a real "no results" answer
//! when the engine's own result container or no-results marker is on it
//! (each engine's `is_<engine>_serp`). Anything else is a page this crate
//! cannot read -- selector drift or an unrecognized wall -- and becomes a
//! typed [`UNRECOGNIZED_MARKUP`] error: never cached, present in
//! `SearchOutput.errors`, logged once per response.

use super::types::{SearchProvider, SearchProviderError, SearchResult};

/// `Upstream.detail` of a drifted page.
pub(crate) const UNRECOGNIZED_MARKUP: &str = "unrecognized markup";

/// Rows from a 2xx page, or the typed drift error when the page has zero
/// rows and is not recognizably a SERP (`is_serp` false).
pub(crate) fn rows_or_drift(
    provider: SearchProvider,
    status: u16,
    body: &str,
    rows: Vec<SearchResult>,
    is_serp: impl FnOnce(&str) -> bool,
) -> Result<Vec<SearchResult>, SearchProviderError> {
    if !rows.is_empty() || is_serp(body) {
        return Ok(rows);
    }
    tracing::warn!(
        engine = provider.id(),
        status,
        body_len = body.len(),
        "search page has neither results nor a no-results marker: unrecognized markup"
    );
    Err(SearchProviderError::Upstream {
        provider,
        detail: UNRECOGNIZED_MARKUP.to_string(),
        status: Some(status),
        retry_after_secs: None,
    })
}

#[cfg(test)]
mod tests {
    //! Per-engine acceptance (#104): a drifted 2xx page is the typed
    //! error; a recognized results or no-results page is an empty `Ok`.
    //! (Neither reaches the cache: `cache::store` refuses zero rows, and
    //! an `Err` never reaches it -- see `fanout::tests::cache`.)

    use crate::web::search::bing::map_bing_response;
    use crate::web::search::brave::map_brave_response;
    use crate::web::search::ddg::map_ddg_response;
    use crate::web::search::startpage::map_startpage_response;
    use crate::web::search::types::{SearchProviderError, SearchResult};
    use crate::web::search::yahoo::map_yahoo_response;

    /// A 2xx page none of the engines recognize (a redesign, or a wall
    /// with no known marker).
    const DRIFTED: &str =
        "<html><body><main class=\"new-layout\"><p>Results</p></main></body></html>";

    type Mapped = Result<Vec<SearchResult>, SearchProviderError>;

    fn assert_drift(engine: &str, mapped: Mapped) {
        match mapped {
            Err(SearchProviderError::Upstream {
                detail,
                status: Some(200),
                retry_after_secs: None,
                ..
            }) if detail == super::UNRECOGNIZED_MARKUP => {}
            other => panic!("{engine}: expected unrecognized markup, got {other:?}"),
        }
    }

    fn assert_empty(engine: &str, mapped: Mapped) {
        match mapped {
            Ok(rows) => assert!(rows.is_empty(), "{engine}: {rows:?}"),
            Err(err) => panic!("{engine}: expected an empty answer, got {err:?}"),
        }
    }

    #[test]
    fn duckduckgo_drift_is_typed_and_its_no_results_page_is_empty() {
        assert_drift("duckduckgo", map_ddg_response(200, DRIFTED, "q", None));
        let no_results =
            include_str!("../../../docs/research/search-engines/fixtures/ddg/no-results.html");
        assert_empty("duckduckgo", map_ddg_response(200, no_results, "q", None));
    }

    #[test]
    fn bing_drift_is_typed_and_an_empty_result_list_is_empty() {
        assert_drift("bing", map_bing_response(200, DRIFTED, "q", None));
        // Bing has no no-results marker of its own: its `b_results` list
        // with no organic row is the recognized empty answer.
        let empty = "<html><body><ol id=\"b_results\"><li class=\"b_ans\">Related searches</li></ol></body></html>";
        assert_empty("bing", map_bing_response(200, empty, "q", None));
        let real = include_str!(
            "../../../docs/research/search-engines/fixtures/bing/Q1-success-mktUS.html"
        );
        assert!(!map_bing_response(200, real, "q", None)
            .expect("rows")
            .is_empty());
    }

    #[test]
    fn brave_shell_without_results_is_drift_and_an_empty_container_is_empty() {
        assert_drift("brave", map_brave_response(200, DRIFTED, "q", None));
        // Brave's SvelteKit shell (served on its 429 wall) has no
        // `id="results"`: at 200 it is unreadable, not "no results".
        let shell = include_str!(
            "../../../docs/research/search-engines/fixtures/brave/Q8-429-blocked.html"
        );
        assert_drift("brave", map_brave_response(200, shell, "q", None));
        let empty = "<html><body><div id=\"results\"></div></body></html>";
        assert_empty("brave", map_brave_response(200, empty, "q", None));
        let real =
            include_str!("../../../docs/research/search-engines/fixtures/brave/Q1-success.html");
        assert!(!map_brave_response(200, real, "q", None)
            .expect("rows")
            .is_empty());
    }

    #[test]
    fn yahoo_degraded_page_is_drift_and_an_empty_container_is_empty() {
        let url = "https://search.yahoo.com/search?p=q";
        assert_drift("yahoo", map_yahoo_response(200, DRIFTED, url, "q", None));
        // Live 2026-10-05: the `zrp` "temporary problems" page came back
        // for real queries too, so it must not read as "nothing exists".
        let degraded =
            include_str!("../../../docs/research/search-engines/fixtures/yahoo/zrp-degraded.html");
        assert_drift("yahoo", map_yahoo_response(200, degraded, url, "q", None));
        let empty = "<html><body><div id=\"results\"><ol class=\"reg\"></ol></div></body></html>";
        assert_empty("yahoo", map_yahoo_response(200, empty, url, "q", None));
        let real =
            include_str!("../../../docs/research/search-engines/fixtures/yahoo/Q1-success.html");
        assert!(!map_yahoo_response(200, real, url, "q", None)
            .expect("rows")
            .is_empty());
    }

    #[test]
    fn startpage_drift_is_typed_and_its_no_results_page_is_empty() {
        let url = "https://www.startpage.com/sp/search";
        assert_drift(
            "startpage",
            map_startpage_response(200, DRIFTED, url, "q", None),
        );
        let no_results = "<html><body><div class=\"w-gl\"><p>Your search did not match any documents.</p></div></body></html>";
        assert_empty(
            "startpage",
            map_startpage_response(200, no_results, url, "q", None),
        );
    }
}
