//! Env-gated agent-loop round trip: bounded termination, never success.
//!
//! Runs only with `GATEWAY_LIVE=1` and `GATEWAY_API_KEY` set. Uses stub
//! tools and the real gateway via `chat_with_tools`; asserts the loop
//! terminates within budget (FINAL *or* bounded abort — live models are
//! non-deterministic). Model order: `GATEWAY_LIVE_MODEL`, else `glm-5p2`,
//! else `mimo-v2.5-free` on 429 (never `glm-5.3-flash` first).

use std::time::Duration;

use web_agent_research::shared::agent_loop::{
    run_loop, LoopBudget, LoopError, LoopInput, ToolRegistry,
};
use web_agent_research::shared::gateway::{Gateway, GatewayConfig, GatewayError};
use web_agent_research::shared::types::synthesis::SynthesisSize;

fn candidate_models() -> Vec<String> {
    let mut models = Vec::new();
    if let Ok(explicit) = std::env::var("GATEWAY_LIVE_MODEL") {
        let explicit = explicit.trim().to_owned();
        if !explicit.is_empty() {
            models.push(explicit);
        }
    }
    for fallback in ["glm-5p2", "mimo-v2.5-free"] {
        if !models.iter().any(|model| model == fallback) {
            models.push(fallback.to_owned());
        }
    }
    models
}

#[tokio::test]
async fn live_loop_terminates_within_budget() -> anyhow::Result<()> {
    if std::env::var("GATEWAY_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping live agent-loop test (GATEWAY_LIVE != 1)");
        return Ok(());
    }
    let base = GatewayConfig::from_env()?;
    let mut last_rate_limit: Option<GatewayError> = None;
    for model in candidate_models() {
        let mut config = base.clone();
        config.model.clone_from(&model);
        let gateway = Gateway::new(config);
        let tools = ToolRegistry::live();
        let input = LoopInput {
            goal: "What is the Obscura headless browser? Answer briefly.".to_owned(),
            size: SynthesisSize::Small,
            allowed_tools: tools
                .tool_names()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        };
        let budget = LoopBudget {
            max_turns: 4,
            max_repairs: 2,
            tool_timeout: Duration::from_secs(20),
            ..LoopBudget::default()
        };
        match run_loop(&gateway, &tools, &input, &budget).await {
            Ok(report) => {
                eprintln!(
                    "live loop FINAL: model={model} turns={} failures={} themes={} citations={} summary_len={}",
                    report.turns_used,
                    report.failures_used,
                    report.synthesis.themes.len(),
                    report.synthesis.citations.len(),
                    report.synthesis.summary.len(),
                );
                assert!(report.turns_used >= 1 && report.turns_used <= 4);
                assert!(!report.synthesis.summary.is_empty());
                assert!(!report.synthesis.themes.is_empty());
                return Ok(());
            }
            Err(LoopError::Gateway(err @ GatewayError::RateLimited { .. })) => {
                eprintln!("live loop model={model} rate-limited: {err}; trying next");
                last_rate_limit = Some(err);
            }
            Err(err) => {
                eprintln!("live loop bounded abort: model={model} err={err}");
                return Ok(());
            }
        }
    }
    Err(anyhow::anyhow!(
        "all live models rate-limited: {}",
        last_rate_limit.map_or("no attempts ran".to_owned(), |e| e.to_string())
    ))
}

#[test]
fn live_loop_tool_names_stay_stable() {
    let tools = ToolRegistry::live();
    assert_eq!(tools.tool_names(), vec!["search", "fetch"]);
}
