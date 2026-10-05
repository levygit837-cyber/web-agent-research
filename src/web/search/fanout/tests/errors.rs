//! Error mapping helpers.

use crate::web::search::ddg::map_ddg_response;
use crate::web::search::fanout::{all_failed_message, map_timeout};
use crate::web::search::startpage::map_startpage_response;
use crate::web::search::types::{SearchProvider, SearchProviderError};

#[test]
fn all_failed_message_format() {
    let msg = all_failed_message(&[
        ("duckduckgo".to_string(), "boom".to_string()),
        ("startpage".to_string(), "bust".to_string()),
    ]);
    assert_eq!(msg, "duckduckgo: boom; startpage: bust");
}

#[test]
fn all_failed_message_lists_each_engine_once() {
    // #104: a multi-query call repeated every engine once per Query.
    let msg = all_failed_message(&[
        (
            "yahoo".to_string(),
            "suspended (challenge), 3412s left".to_string(),
        ),
        (
            "bing".to_string(),
            "suspended (challenge), 900s left".to_string(),
        ),
        (
            "yahoo".to_string(),
            "suspended (challenge), 3412s left".to_string(),
        ),
        ("bing".to_string(), "Bing blocked the request".to_string()),
    ]);
    assert_eq!(
        msg,
        "yahoo: suspended (challenge), 3412s left; bing: suspended (challenge), 900s left / Bing blocked the request"
    );
}

#[test]
fn timeout_beats_challenge() {
    let err = map_timeout(SearchProvider::DuckDuckGo, "q");
    assert_eq!(err.http_status(), 504);
    assert_eq!(err.code(), "timeout");
}

#[test]
fn http_500_maps_503() {
    let err = map_ddg_response(500, "boom", "q", None).unwrap_err();
    assert_eq!(err.http_status(), 503);
    let err = map_startpage_response(500, "boom", "https://x/", "q", None).unwrap_err();
    assert_eq!(err.http_status(), 503);
}

#[test]
fn validation_status_codes() {
    assert_eq!(SearchProviderError::EmptyQuery.http_status(), 400);
    assert_eq!(SearchProviderError::EmptyQuery.code(), "empty_query");
    assert_eq!(
        SearchProviderError::TooManyQueries { got: 9, max: 8 }.http_status(),
        400
    );
    assert_eq!(
        SearchProviderError::AllFailed {
            failures: "a: b".to_string(),
            all_challenged: false,
        }
        .http_status(),
        503
    );
    assert_eq!(SearchProviderError::EmptyQuery.provider(), None);
}
