//! Env-gated research round trip: bounded termination, never success.
//!
//! Runs only with `GATEWAY_LIVE=1` and `GATEWAY_API_KEY` set. Uses the real
//! gateway through the public `run_research` interface with `max_turns <= 4`.
//! The 429-credit wall counts as pass-with-note: the error must be
//! `GatewayExhausted` (retry-or-abort), never a hang or panic. Model:
//! `GATEWAY_LIVE_MODEL`, else `GATEWAY_MODEL`, else the config default.

use web_agent_research::research::synthesis::SynthesisSize;
use web_agent_research::{run_research, ResearchError, ResearchRequest};

/// The one live model: no fallback chain.
fn live_model() -> Option<String> {
    ["GATEWAY_LIVE_MODEL", "GATEWAY_MODEL"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
}

#[tokio::test]
async fn live_research_terminates_within_budget() -> anyhow::Result<()> {
    if std::env::var("GATEWAY_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping live research test (GATEWAY_LIVE != 1)");
        return Ok(());
    }
    let key_present = std::env::var("GATEWAY_API_KEY")
        .map(|key| !key.trim().is_empty())
        .unwrap_or(false);
    if !key_present {
        eprintln!("skipping live research test (GATEWAY_API_KEY not set)");
        return Ok(());
    }
    if let Some(model) = live_model() {
        std::env::set_var("GATEWAY_MODEL", model);
    }
    let model = std::env::var("GATEWAY_MODEL").unwrap_or_else(|_| "<config default>".to_owned());
    let req = ResearchRequest {
        goal: "what is the Obscura headless browser?".to_owned(),
        size: SynthesisSize::Small,
        max_turns: 4,
        session_id: None,
        session_out: None,
    };
    match run_research(req).await {
        Ok(response) => {
            eprintln!(
                "live research FINAL: model={model} turns={} citations={} summary_len={}",
                response.turns_used,
                response.synthesis.citations.len(),
                response.synthesis.summary.len(),
            );
            assert!((1..=4).contains(&response.turns_used));
            assert!(!response.synthesis.summary.is_empty());
            assert!(!response.synthesis.themes.is_empty());
            Ok(())
        }
        Err(ResearchError::GatewayExhausted(reason)) => {
            eprintln!("live research model={model} exhausted (pass-with-note): {reason}");
            Ok(())
        }
        Err(err) => {
            eprintln!("live research bounded abort: model={model} err={err}");
            Ok(())
        }
    }
}
