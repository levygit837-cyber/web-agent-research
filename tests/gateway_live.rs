//! Live gateway round trip: env-gated so CI without a gateway stays green.
//!
//! Runs only with `GATEWAY_LIVE=1` and `GATEWAY_API_KEY` set. Tries a working
//! model first (`GATEWAY_LIVE_MODEL`, else `glm-5p2`, else `mimo-v2.5-free` on
//! 429) and makes exactly one attempt per model — never a probe loop.

use web_agent_research::shared::gateway::{ChatMessage, Gateway, GatewayConfig, GatewayError};

fn candidate_models() -> Vec<String> {
    let mut models = Vec::new();
    if let Ok(explicit) = std::env::var("GATEWAY_LIVE_MODEL") {
        let explicit = explicit.trim().to_owned();
        if !explicit.is_empty() {
            models.push(explicit);
        }
    }
    for fallback in ["glm-5p2", "mimo-v2.5-free"] {
        if !models.iter().any(|m| m == fallback) {
            models.push(fallback.to_owned());
        }
    }
    models
}

#[tokio::test]
async fn live_round_trip() -> anyhow::Result<()> {
    if std::env::var("GATEWAY_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping live gateway test (GATEWAY_LIVE != 1)");
        return Ok(());
    }
    let mut base = GatewayConfig::from_env()?;
    let mut last_rate_limit: Option<GatewayError> = None;
    for model in candidate_models() {
        base.model.clone_from(&model);
        let gateway = Gateway::new(base.clone());
        match gateway
            .chat(&[ChatMessage {
                role: "user".to_owned(),
                content: "Reply with exactly: gateway ok".to_owned(),
            }])
            .await
        {
            Ok(reply) => {
                eprintln!(
                    "live reply: model={model} finish_reason={:?} thinking_len={} usage={:?}",
                    reply.finish_reason,
                    reply.thinking.len(),
                    reply.usage,
                );
                assert!(
                    reply.output.contains("gateway ok"),
                    "expected 'gateway ok' in output, got {:?}",
                    reply.output
                );
                assert!(
                    reply.finish_reason.is_some(),
                    "expected a finish_reason, got None"
                );
                return Ok(());
            }
            Err(err @ GatewayError::RateLimited { .. }) => {
                eprintln!("live attempt with model={model} rate-limited: {err}; trying next");
                last_rate_limit = Some(err);
            }
            Err(err) => return Err(anyhow::anyhow!("live gateway chat failed: {err}")),
        }
    }
    Err(anyhow::anyhow!(
        "all live models rate-limited: {}",
        last_rate_limit.map_or("no attempts ran".to_owned(), |e| e.to_string())
    ))
}
