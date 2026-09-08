//! Hermetic suite for the Obscura fetch integration (issue #14).
//!
//! The engine under test points at `tests/fixtures/obscura-fake.sh`, a shell
//! double dispatching on the positional URL ($2). No real `obscura` binary is
//! needed; the single live check lives in `tests/live_obscura.rs` (ignored).

use std::path::PathBuf;
use std::time::Duration;
use web_agent_research::shared::obscura::browser::{FetchError, Obscura};
use web_agent_research::shared::session::Evidence;
use web_agent_research::shared::tools::fetch::{
    fetch_tool, fetch_tool_schema, FetchInput, FETCH_TOOL_NAME,
};

fn fake_engine() -> Obscura {
    Obscura::new(fixture_binary(), Duration::from_secs(10))
}

fn fixture_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("obscura-fake.sh")
}

/// Extract the `url` field from a `Blocked` / `EmptyBody` / `Timeout` /
/// `CommandFailed` error for URL-carry assertions.
fn error_url(err: &FetchError) -> &str {
    match err {
        FetchError::InvalidUrl { .. } => panic!("expected url-carrying error"),
        FetchError::EmptyBody { url }
        | FetchError::Blocked { url, .. }
        | FetchError::Timeout { url, .. }
        | FetchError::CommandFailed { url, .. } => url,
    }
}

#[test]
fn evidence_new_stamps_rfc3339_collection_time() {
    let ev = Evidence::new(
        "https://example.com/page".to_string(),
        "# Title".to_string(),
    );
    assert_eq!(ev.source_url, "https://example.com/page");
    assert_eq!(ev.markdown, "# Title");
    // RFC 3339 UTC, second precision: YYYY-MM-DDTHH:MM:SSZ.
    assert_eq!(
        ev.collected_at.len(),
        20,
        "collected_at = {}",
        ev.collected_at
    );
    assert!(ev.collected_at.ends_with('Z'));
    assert_eq!(&ev.collected_at[10..11], "T");
}

#[test]
fn evidence_round_trips_through_json() {
    let ev = Evidence::new("https://example.com/a".to_string(), "body".to_string());
    let back: Evidence = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(ev, back);
}

// --- Extractor: happy path + URL validation ---

#[tokio::test]
async fn fetch_markdown_returns_trimmed_markdown_for_ok_url() {
    let engine = fake_engine();
    let fetched = engine
        .fetch_markdown("https://example.com/page")
        .await
        .unwrap();
    assert_eq!(fetched.url, "https://example.com/page");
    assert_eq!(
        fetched.markdown,
        "# Title\n\nbody for https://example.com/page"
    );
}

#[tokio::test]
async fn fetch_markdown_trims_surrounding_whitespace() {
    let engine = fake_engine();
    let fetched = engine
        .fetch_markdown("https://example.com/pad")
        .await
        .unwrap();
    assert_eq!(fetched.markdown, "# Title\n\nbody");
}

#[tokio::test]
async fn fetch_markdown_rejects_non_url_input_without_spawning() {
    for input in ["not-a-url", "", "ftp://x/y", "/relative/path"] {
        let err = fake_engine().fetch_markdown(input).await.unwrap_err();
        assert!(
            matches!(err, FetchError::InvalidUrl { .. }),
            "{input:?} -> {err}"
        );
        assert_eq!(
            err.to_string(),
            format!("invalid url: {input} (expected absolute http(s) URL)")
        );
    }
}

// --- Extractor: error matrix ---

#[tokio::test]
async fn fetch_markdown_maps_empty_stdout_to_empty_body() {
    let err = fake_engine()
        .fetch_markdown("https://example.com/empty")
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::EmptyBody { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        "empty body: https://example.com/empty returned no markdown"
    );
}

#[tokio::test]
async fn fetch_markdown_maps_blocked_markers_regardless_of_exit_code() {
    let err = fake_engine()
        .fetch_markdown("https://example.com/blocked")
        .await
        .unwrap_err();
    let FetchError::Blocked { url, reason } = &err else {
        panic!("expected Blocked, got {err}");
    };
    assert_eq!(url, "https://example.com/blocked");
    assert!(reason.contains("bot denied"), "reason = {reason}");
    assert_eq!(err.to_string(), format!("blocked: {url}: {reason}"));
}

#[tokio::test]
async fn fetch_markdown_ignores_robots_chatter_on_success() {
    let fetched = fake_engine()
        .fetch_markdown("https://example.com/robots-page")
        .await
        .unwrap();
    assert!(fetched.markdown.starts_with("# Title"));
}

#[tokio::test]
async fn fetch_markdown_ignores_digit_glued_403_lookalikes() {
    let fetched = fake_engine()
        .fetch_markdown("https://example.com/bignum")
        .await
        .unwrap();
    assert!(fetched.markdown.starts_with("# Title"));
}

#[tokio::test]
async fn fetch_markdown_maps_slow_fixture_to_timeout() {
    let engine = Obscura::new(fixture_binary(), Duration::from_millis(200));
    let err = engine
        .fetch_markdown("https://example.com/slow")
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Timeout { .. }), "{err}");
    assert_eq!(error_url(&err), "https://example.com/slow");
    assert_eq!(
        err.to_string(),
        "timeout: https://example.com/slow after 200ms"
    );
}

#[tokio::test]
async fn fetch_markdown_maps_plain_nonzero_exit_to_command_failed() {
    let err = fake_engine()
        .fetch_markdown("https://example.com/fail")
        .await
        .unwrap_err();
    let FetchError::CommandFailed { url, detail } = &err else {
        panic!("expected CommandFailed, got {err}");
    };
    assert_eq!(url, "https://example.com/fail");
    assert!(detail.contains("boom"), "detail = {detail}");
    assert_eq!(err.to_string(), format!("command failed: {url}: {detail}"));
}

// --- Fetch error Display contract ---

#[test]
fn fetch_error_display_carries_url_and_cause() {
    let timeout = FetchError::Timeout {
        url: "https://example.com/x".to_string(),
        after: Duration::from_secs(60),
    };
    assert_eq!(
        timeout.to_string(),
        "timeout: https://example.com/x after 60s"
    );
    let blocked = FetchError::Blocked {
        url: "https://example.com/x".to_string(),
        reason: "captcha required".to_string(),
    };
    assert_eq!(
        blocked.to_string(),
        "blocked: https://example.com/x: captcha required"
    );
}

// --- Fetch tool: schema, input parsing, delegation ---

#[test]
fn fetch_tool_schema_matches_mandatory_agent_loop_contract() {
    let schema = fetch_tool_schema();
    let expected = serde_json::json!({
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
    });
    assert_eq!(schema, expected);
    assert_eq!(schema["name"], FETCH_TOOL_NAME);
    assert_eq!(schema["parameters"]["required"], serde_json::json!(["url"]));
    assert_eq!(
        schema["parameters"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(schema["parameters"]["additionalProperties"], false);
}

#[test]
fn fetch_input_parse_rejects_non_object_and_non_string_url() {
    // Non-object.
    let err = FetchInput::parse(&serde_json::json!("https://example.com")).unwrap_err();
    assert!(matches!(err, FetchError::InvalidUrl { .. }), "{err}");
    // Missing url.
    let err = FetchInput::parse(&serde_json::json!({})).unwrap_err();
    assert!(matches!(err, FetchError::InvalidUrl { .. }), "{err}");
    // Non-string url.
    let err = FetchInput::parse(&serde_json::json!({"url": 42})).unwrap_err();
    assert!(matches!(err, FetchError::InvalidUrl { .. }), "{err}");
    // Happy parse.
    let input = FetchInput::parse(&serde_json::json!({"url": "https://example.com"})).unwrap();
    assert_eq!(input.url, "https://example.com");
}

#[tokio::test]
async fn fetch_tool_wraps_engine_markdown_in_evidence() {
    let engine = fake_engine();
    let input = serde_json::json!({"url": "https://example.com/page"});
    let evidence = fetch_tool(&input, &engine).await.unwrap();
    assert_eq!(evidence.source_url, "https://example.com/page");
    assert_eq!(
        evidence.markdown,
        "# Title\n\nbody for https://example.com/page"
    );
    assert!(evidence.collected_at.ends_with('Z'));
}

#[tokio::test]
async fn fetch_tool_forwards_engine_errors() {
    let engine = fake_engine();
    let input = serde_json::json!({"url": "not-a-url"});
    let err = fetch_tool(&input, &engine).await.unwrap_err();
    assert!(matches!(err, FetchError::InvalidUrl { .. }), "{err}");
}
