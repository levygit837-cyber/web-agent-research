//! Env-gated agent-loop round trip: bounded termination, never success.
//!
//! Runs only with `GATEWAY_LIVE=1` and `GATEWAY_API_KEY` set. Uses the real
//! tools and the real gateway via `chat_with_tools`; asserts the loop
//! terminates within budget (FINAL *or* bounded abort — live models are
//! non-deterministic). Model: `GATEWAY_LIVE_MODEL`, else `GATEWAY_MODEL`,
//! else the config default. One model, no fallback chain.

use std::time::Duration;

use web_agent_research::llm::{Gateway, GatewayConfig};
use web_agent_research::research::agent_loop::{run_loop, LoopBudget, LoopInput, ToolRegistry};
use web_agent_research::research::synthesis::SynthesisSize;
use web_agent_research::web::fetch::Fetcher;
use web_agent_research::web::search::tool::Searcher;

#[tokio::test]
async fn live_loop_terminates_within_budget() -> anyhow::Result<()> {
    if std::env::var("GATEWAY_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping live agent-loop test (GATEWAY_LIVE != 1)");
        return Ok(());
    }
    let mut config = GatewayConfig::from_env()?;
    if let Some(model) = std::env::var("GATEWAY_LIVE_MODEL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        config.model = model;
    }
    let model = config.model.clone();
    let gateway = Gateway::new(config);
    let tools = ToolRegistry::new(Searcher::new(), Fetcher::new());
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
        }
        Err(err) => eprintln!("live loop bounded abort: model={model} err={err}"),
    }
    Ok(())
}

#[test]
fn live_loop_tool_names_stay_stable() {
    let tools = ToolRegistry::new(Searcher::new(), Fetcher::new());
    assert_eq!(tools.tool_names(), vec!["search", "fetch"]);
}
