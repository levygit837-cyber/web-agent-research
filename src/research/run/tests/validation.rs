//! Pre-flight validation: runs that must fail `NotConfigured` before any
//! I/O.

use super::super::*;
use super::helpers::temp_session_out;
use crate::research::synthesis::SynthesisSize;
use crate::test_support::EnvGuard;

#[tokio::test]
async fn empty_goal_is_not_configured_without_io() {
    let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
    std::env::set_var("GATEWAY_API_KEY", "dummy");
    let session_out = temp_session_out("empty-goal");
    let req = ResearchRequest {
        goal: "   ".to_owned(),
        size: SynthesisSize::Small,
        max_turns: 8,
        session_id: Some("test-empty".to_owned()),
        session_out: Some(session_out.clone()),
    };
    let err = run_research(req).await.expect_err("empty goal must fail");
    assert!(
        matches!(err, ResearchError::NotConfigured(_)),
        "empty goal must be NotConfigured, got {err:?}"
    );
    assert!(!session_out.exists(), "NotConfigured must not attempt I/O");
}

#[tokio::test]
async fn zero_max_turns_is_not_configured() {
    let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
    std::env::set_var("GATEWAY_API_KEY", "dummy");
    let req = ResearchRequest {
        goal: "What is the Obscura headless browser?".to_owned(),
        size: SynthesisSize::Small,
        max_turns: 0,
        session_id: Some("test-zero".to_owned()),
        session_out: Some(temp_session_out("zero-turns")),
    };
    let err = run_research(req).await.expect_err("max_turns=0 must fail");
    assert!(
        matches!(err, ResearchError::NotConfigured(_)),
        "max_turns=0 must be NotConfigured, got {err:?}"
    );
}

/// A window the reply reserve swallows is a configuration error
/// (exit 2) naming the variable, raised before any I/O.
#[tokio::test]
async fn a_context_window_smaller_than_the_reply_reserve_is_not_configured() {
    let _guard = EnvGuard::lock(vec![
        "GATEWAY_BASE_URL",
        "GATEWAY_API_KEY",
        "GATEWAY_MODEL",
        "GATEWAY_CONTEXT_WINDOW",
    ]);
    std::env::set_var("GATEWAY_API_KEY", "dummy");
    std::env::set_var("GATEWAY_CONTEXT_WINDOW", "4000");
    let session_out = temp_session_out("tiny-window");
    let req = ResearchRequest {
        goal: "What is the Obscura headless browser?".to_owned(),
        size: SynthesisSize::Small,
        max_turns: 8,
        session_id: Some("test-tiny-window".to_owned()),
        session_out: Some(session_out.clone()),
    };
    let err = run_research(req)
        .await
        .expect_err("a 4000-token window cannot hold the reply reserve");
    match err {
        ResearchError::NotConfigured(reason) => {
            assert!(reason.contains("GATEWAY_CONTEXT_WINDOW"), "{reason}");
        }
        other => panic!("expected NotConfigured, got {other:?}"),
    }
    assert!(!session_out.exists(), "NotConfigured must not attempt I/O");
}
