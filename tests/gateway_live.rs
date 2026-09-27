//! Live gateway round trip: env-gated so CI without a gateway stays green.
//!
//! Runs only with `GATEWAY_LIVE=1` and `GATEWAY_API_KEY` set. Uses the one
//! configured model (`GATEWAY_LIVE_MODEL`, else `GATEWAY_MODEL`, else the
//! config default) and makes exactly one attempt — never a probe loop.

use web_agent_research::llm::{ChatMessage, Gateway, GatewayConfig, ToolChoice, ToolDef};

fn live_config() -> anyhow::Result<GatewayConfig> {
    let mut config = GatewayConfig::from_env()?;
    if let Some(model) = std::env::var("GATEWAY_LIVE_MODEL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        config.model = model;
    }
    Ok(config)
}

#[tokio::test]
async fn live_round_trip() -> anyhow::Result<()> {
    if std::env::var("GATEWAY_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping live gateway test (GATEWAY_LIVE != 1)");
        return Ok(());
    }
    let config = live_config()?;
    let model = config.model.clone();
    let reply = Gateway::new(config)
        .chat(&[ChatMessage::user("Reply with exactly: gateway ok")])
        .await
        .map_err(|err| anyhow::anyhow!("live gateway chat failed (model={model}): {err}"))?;
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
    Ok(())
}

#[tokio::test]
async fn live_tools_round_trip() -> anyhow::Result<()> {
    if std::env::var("GATEWAY_LIVE").as_deref() != Ok("1") {
        eprintln!("skipping live tools test (GATEWAY_LIVE != 1)");
        return Ok(());
    }
    let config = live_config()?;
    let model = config.model.clone();
    let tools = vec![ToolDef {
        name: "get_time".to_owned(),
        description: "Returns the current time.".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }];
    let messages = [ChatMessage::user("What time is it? Use the get_time tool.")];
    let reply = Gateway::new(config)
        .chat_with_tools(&messages, &tools, Some(ToolChoice::Auto))
        .await
        .map_err(|err| anyhow::anyhow!("live gateway tools chat failed (model={model}): {err}"))?;
    eprintln!(
        "live tools reply: model={model} finish_reason={:?} tool_calls={} thinking_len={} usage={:?}",
        reply.finish_reason,
        reply.tool_calls.len(),
        reply.thinking.len(),
        reply.usage,
    );
    for call in &reply.tool_calls {
        eprintln!(
            "live tool_call: id={} name={} arguments={}",
            call.id, call.name, call.arguments
        );
    }
    assert!(
        !reply.tool_calls.is_empty(),
        "expected at least one tool call, got {:?}",
        reply
    );
    let call = reply
        .tool_calls
        .iter()
        .find(|c| c.name == "get_time")
        .expect("expected a get_time tool call");
    call.parsed_arguments()
        .map_err(|err| anyhow::anyhow!("get_time arguments are not valid JSON: {err}"))?;
    Ok(())
}
