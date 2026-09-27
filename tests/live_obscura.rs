//! The single live check for issue #14: real `obscura` binary, ignored by
//! default. Binary absent = skip (`Ok(())`), never a failure. Run explicitly
//! where the binary exists: `cargo test -- --ignored`.

use web_agent_research::web::fetch::{FetchError, Obscura};

#[tokio::test]
#[ignore]
async fn live_obscura_fetch_returns_markdown() {
    let engine = Obscura::default();
    match engine.fetch_markdown("https://example.com").await {
        Ok(fetched) => {
            assert_eq!(fetched.url, "https://example.com/");
            assert!(
                !fetched.markdown.trim().is_empty(),
                "live fetch returned empty markdown"
            );
        }
        Err(FetchError::FallbackUnavailable { .. }) => {
            // Binary absent: skip, not failure.
        }
        Err(other) => panic!("live fetch failed unexpectedly: {other}"),
    }
}
