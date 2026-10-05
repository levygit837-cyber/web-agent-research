//! Hermetic suite for the Obscura fallback engine and the fetch tool seam.
//!
//! The engine under test points at `tests/fixtures/obscura-fake.sh`, a shell
//! double dispatching on the positional URL ($2). No real `obscura` binary is
//! needed; the single live check lives in `tests/live_obscura.rs` (ignored).

use std::path::PathBuf;
use std::time::Duration;
use web_agent_research::web::fetch::tool::{fetch_tool_schema, FetchInput, FETCH_TOOL_NAME};
use web_agent_research::web::fetch::{Evidence, FetchError, FetchPath, Obscura};

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
        FetchError::InvalidUrl { .. } | FetchError::InvalidPart { .. } => {
            panic!("expected url-carrying error")
        }
        FetchError::EmptyBody { url }
        | FetchError::Egress { url, .. }
        | FetchError::Blocked { url, .. }
        | FetchError::Timeout { url, .. }
        | FetchError::CommandFailed { url, .. }
        | FetchError::Http { url, .. }
        | FetchError::FallbackUnavailable { url, .. } => url,
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

#[test]
fn evidence_without_fetch_path_omits_it_from_json() {
    let ev = Evidence::new("https://example.com/a".to_string(), "body".to_string());
    let json = serde_json::to_value(&ev).unwrap();
    assert!(
        json.as_object().unwrap().get("fetch_path").is_none(),
        "fetch_path should be omitted when None: {json}"
    );
}

#[test]
fn pre_fetch_path_evidence_json_still_deserializes() {
    // Shape of a Session JSONL row written before #31 added `fetch_path`.
    let old_row = serde_json::json!({
        "source_url": "https://example.com/old",
        "collected_at": "2026-01-01T00:00:00Z",
        "markdown": "legacy body"
    });
    let ev: Evidence = serde_json::from_value(old_row).unwrap();
    assert_eq!(ev.source_url, "https://example.com/old");
    assert_eq!(ev.fetch_path, None);
}

#[test]
fn evidence_fetch_path_round_trips_as_lowercase_json() {
    let mut ev = Evidence::new("https://example.com/a".to_string(), "body".to_string());
    ev.fetch_path = Some(FetchPath::Static);
    let json = serde_json::to_value(&ev).unwrap();
    assert_eq!(json["fetch_path"], "static");
    let back: Evidence = serde_json::from_value(json).unwrap();
    assert_eq!(back.fetch_path, Some(FetchPath::Static));

    ev.fetch_path = Some(FetchPath::Browser);
    assert_eq!(serde_json::to_value(&ev).unwrap()["fetch_path"], "browser");
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

/// #97: Obscura output is read incrementally and capped at 5 MiB per
/// stream; an endless writer is killed with a typed error well before the
/// 10 s engine timeout instead of buffering without bound.
#[tokio::test]
async fn endless_obscura_output_is_capped_and_killed() {
    for (route, stream) in [("flood", "stdout"), ("errflood", "stderr")] {
        let started = std::time::Instant::now();
        let err = fake_engine()
            .fetch_markdown(&format!("https://example.com/{route}"))
            .await
            .unwrap_err();
        let FetchError::CommandFailed { detail, .. } = &err else {
            panic!("{route}: expected CommandFailed, got {err:?}");
        };
        assert!(
            detail.contains(&format!("obscura {stream} exceeds 5242880 bytes")),
            "{route}: {detail}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{route}: cap must trip before the engine timeout"
        );
    }
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
    assert_eq!(schema["name"], FETCH_TOOL_NAME);
    assert_eq!(schema["parameters"]["type"], "object");
    assert_eq!(schema["parameters"]["required"], serde_json::json!(["url"]));
    let properties = schema["parameters"]["properties"]
        .as_object()
        .expect("parameters.properties object");
    assert_eq!(properties.len(), 2);
    assert_eq!(properties["url"]["type"], "string");
    assert_eq!(properties["part"]["type"], "integer");
    assert_eq!(properties["part"]["minimum"], 1);
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
async fn missing_binary_is_fallback_unavailable() {
    let engine = Obscura::new(
        PathBuf::from("/nonexistent/obscura"),
        Duration::from_secs(1),
    );
    let err = engine
        .fetch_markdown("https://example.com/page")
        .await
        .unwrap_err();
    assert!(
        matches!(err, FetchError::FallbackUnavailable { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn obscura_path_result_went_through_the_cleanup_pass() {
    // #79: proves `Obscura::fetch_markdown` calls the shared cleanup, not
    // just that `clean_markdown` works in isolation (that's covered unit-
    // side). The fixture's `*noisy*` route dumps a raw data-URI image and a
    // tracked link; the data URI must never reach the markdown, and the
    // tracked link must keep its destination without the tracking param.
    let fetched = fake_engine()
        .fetch_markdown("https://example.com/noisy")
        .await
        .unwrap();
    assert!(
        !fetched.markdown.to_ascii_lowercase().contains("base64"),
        "data URI leaked: {}",
        fetched.markdown
    );
    assert!(
        fetched.markdown.contains("inline chart"),
        "alt text missing: {}",
        fetched.markdown
    );
    assert!(
        fetched
            .markdown
            .contains("[reference](https://example.org/ref?id=3)"),
        "tracking param not stripped: {}",
        fetched.markdown
    );
    assert!(!fetched.markdown.contains("utm_source"));
}
