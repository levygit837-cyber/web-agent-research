//! Anthropic Messages API wire format (`POST {base}/messages`).
//!
//! Maps the format-neutral [`ChatMessage`] transcript onto Messages content
//! blocks and parses a Messages response back into [`LlmReply`]:
//!
//! - every `system` message → the top-level `system` text-block array;
//! - an assistant tool turn → one `assistant` message: its
//!   [`ReplayBlocks`] (`thinking`/`redacted_thinking`, verbatim and first),
//!   optional `text`, then one `tool_use` block per call;
//! - consecutive `tool` messages → one `user` message holding only
//!   `tool_result` blocks, so results always sit first and immediately
//!   follow their `tool_use` turn, and every id is answered.
//!
//! Prompt caching uses explicit `cache_control: {"type": "ephemeral"}`
//! breakpoints on the last tool, the last system block, and the last block
//! of the final message (3 of the 4 allowed). The cache prefix order is
//! `tools → system → messages`, and the transcript is append-only, so each
//! turn reads the prefix the previous turn wrote.

use serde::Deserialize;

use super::client::GatewayError;
use super::config::{ReasoningEffort, ResolvedParams};
use super::reply::{LlmReply, RequestedToolCall, TokenUsage};
use super::request::{
    merge_extra_body, ChatMessage, ReplayBlocks, ThinkingParam, ToolChoice, ToolDef,
};
use super::response::null_to_default;

/// `anthropic-version` header value; the only current API version.
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `max_tokens` is required on the Messages API; used when
/// `GATEWAY_MAX_TOKENS` is unset. An enabled thinking budget is added on
/// top because thinking tokens count against `max_tokens`.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Smallest `budget_tokens` the Messages API accepts.
pub(crate) const MIN_THINKING_BUDGET: u32 = 1024;

fn ephemeral() -> serde_json::Value {
    serde_json::json!({"type": "ephemeral"})
}

/// Messages API `output_config.effort` value. `none`/`minimal` have no
/// Messages equivalent; `GatewayConfig::from_env` rejects them for this
/// format, so this only sees the five supported levels.
fn effort_str(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

fn text_block(text: &str) -> serde_json::Value {
    serde_json::json!({"type": "text", "text": text})
}

/// `Some(text)` unless it is empty or whitespace-only: the Messages API
/// rejects such text blocks with 400 ("text content blocks must contain
/// non-whitespace text"), so they are dropped instead of sent.
fn non_blank(text: &str) -> Option<&str> {
    (!text.trim().is_empty()).then_some(text)
}

/// Tool-call arguments as the JSON object `tool_use.input` requires. The
/// model sent them as an object; a non-object or unparsable string (only
/// possible from an OpenAI-shaped history) degrades to `{}`.
fn tool_input(arguments: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => serde_json::json!({}),
    }
}

/// Split the transcript into the top-level `system` blocks and the
/// `messages` array.
fn map_messages(messages: &[ChatMessage]) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let mut system = Vec::new();
    let mut wire: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
    let mut pending_results: Vec<serde_json::Value> = Vec::new();

    let flush = |wire: &mut Vec<serde_json::Value>, pending: &mut Vec<serde_json::Value>| {
        if !pending.is_empty() {
            wire.push(serde_json::json!({
                "role": "user",
                "content": std::mem::take(pending),
            }));
        }
    };

    for message in messages {
        match message.role.as_str() {
            "tool" => {
                pending_results.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.as_deref().unwrap_or_default(),
                    "content": message.content_text(),
                }));
                continue;
            }
            "system" => {
                flush(&mut wire, &mut pending_results);
                if let Some(text) = non_blank(message.content_text()) {
                    system.push(text_block(text));
                }
            }
            "assistant" => {
                flush(&mut wire, &mut pending_results);
                let mut content: Vec<serde_json::Value> = message.replay.blocks.clone();
                if let Some(text) = non_blank(message.content_text()) {
                    content.push(text_block(text));
                }
                for call in message.tool_calls.iter().flatten() {
                    content.push(serde_json::json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": tool_input(&call.arguments),
                    }));
                }
                if !content.is_empty() {
                    wire.push(serde_json::json!({"role": "assistant", "content": content}));
                }
            }
            _ => {
                flush(&mut wire, &mut pending_results);
                if let Some(text) = non_blank(message.content_text()) {
                    wire.push(serde_json::json!({
                        "role": "user",
                        "content": [text_block(text)],
                    }));
                }
            }
        }
    }
    flush(&mut wire, &mut pending_results);
    (system, wire)
}

/// Put `cache_control` on the last content block of `message`, if any.
fn mark_last_block(message: Option<&mut serde_json::Value>) {
    if let Some(block) = message
        .and_then(|m| m.get_mut("content"))
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
        .and_then(serde_json::Value::as_object_mut)
    {
        block.insert("cache_control".to_owned(), ephemeral());
    }
}

/// Full Messages request body, `extra_body` merged under the typed keys.
pub(crate) fn request_body(
    params: &ResolvedParams,
    messages: &[ChatMessage],
    tools: Option<&[ToolDef]>,
    tool_choice: Option<&ToolChoice>,
    extra: Option<&serde_json::Value>,
) -> serde_json::Value {
    let (mut system, mut wire) = map_messages(messages);
    if let Some(block) = system.last_mut().and_then(serde_json::Value::as_object_mut) {
        block.insert("cache_control".to_owned(), ephemeral());
    }
    mark_last_block(wire.last_mut());

    let thinking = params.thinking_budget.map(ThinkingParam::from_budget);
    let thinking_tokens = match thinking {
        Some(ThinkingParam::Enabled { budget_tokens }) => budget_tokens,
        _ => 0,
    };
    let max_tokens = params
        .max_tokens
        .unwrap_or_else(|| DEFAULT_MAX_TOKENS.saturating_add(thinking_tokens));

    let mut body = serde_json::Map::new();
    body.insert("model".to_owned(), params.model.clone().into());
    body.insert("max_tokens".to_owned(), max_tokens.into());
    if !system.is_empty() {
        body.insert("system".to_owned(), system.into());
    }
    body.insert("messages".to_owned(), wire.into());
    if let Some(tools) = tools.filter(|t| !t.is_empty()) {
        let mut defs: Vec<serde_json::Value> = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        if let Some(last) = defs.last_mut().and_then(serde_json::Value::as_object_mut) {
            last.insert("cache_control".to_owned(), ephemeral());
        }
        body.insert("tools".to_owned(), defs.into());
    }
    if let Some(choice) = tool_choice {
        let choice = match choice {
            ToolChoice::Auto => serde_json::json!({"type": "auto"}),
            ToolChoice::None => serde_json::json!({"type": "none"}),
            ToolChoice::Named(name) => serde_json::json!({"type": "tool", "name": name}),
        };
        body.insert("tool_choice".to_owned(), choice);
    }
    if let Some(thinking) = thinking {
        body.insert(
            "thinking".to_owned(),
            serde_json::to_value(thinking).expect("ThinkingParam serializes"),
        );
    }
    if let Some(effort) = params.reasoning_effort {
        body.insert(
            "output_config".to_owned(),
            serde_json::json!({"effort": effort_str(effort)}),
        );
    }
    merge_extra_body(serde_json::Value::Object(body), extra)
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessagesResponse {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "null_to_default")]
    content: Vec<serde_json::Value>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<MessagesUsage>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct MessagesUsage {
    #[serde(default, deserialize_with = "null_to_default")]
    input_tokens: u64,
    #[serde(default, deserialize_with = "null_to_default")]
    output_tokens: u64,
    #[serde(default, deserialize_with = "null_to_default")]
    cache_creation_input_tokens: u64,
    #[serde(default, deserialize_with = "null_to_default")]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct OutputDetails {
    #[serde(default, deserialize_with = "null_to_default")]
    thinking_tokens: u64,
}

/// Anthropic `stop_reason` → the OpenAI `finish_reason` vocabulary the loop
/// already reads. Unknown values pass through verbatim.
fn finish_reason(stop_reason: &str) -> String {
    match stop_reason {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" | "model_context_window_exceeded" => "length",
        "tool_use" => "tool_calls",
        other => other,
    }
    .to_owned()
}

fn string_chars(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.chars().count(),
        serde_json::Value::Array(items) => items.iter().map(string_chars).sum(),
        serde_json::Value::Object(map) => map.values().map(string_chars).sum(),
        _ => 0,
    }
}

pub(crate) fn parse_response(resp: MessagesResponse) -> Result<LlmReply, GatewayError> {
    if resp.kind.as_deref() == Some("error") {
        let detail = resp
            .error
            .as_ref()
            .and_then(|e| e.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error body on a 2xx response")
            .to_owned();
        return Err(GatewayError::Parse(detail));
    }
    let mut output = String::new();
    let mut thinking: Vec<String> = Vec::new();
    let mut replay = ReplayBlocks::default();
    let mut tool_calls = Vec::new();
    for block in resp.content {
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                    output.push_str(text);
                }
            }
            Some("thinking") => {
                if let Some(text) = block
                    .get("thinking")
                    .and_then(serde_json::Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    thinking.push(text.to_owned());
                }
                replay.chars += string_chars(&block);
                replay.blocks.push(block);
            }
            Some("redacted_thinking") => {
                replay.chars += string_chars(&block);
                replay.blocks.push(block);
            }
            Some("tool_use") => {
                let field = |key: &str| {
                    block
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                tool_calls.push(RequestedToolCall {
                    id: field("id"),
                    name: field("name"),
                    arguments: block
                        .get("input")
                        .map_or_else(|| "{}".to_owned(), serde_json::Value::to_string),
                });
            }
            _ => {}
        }
    }
    if resp.stop_reason.as_deref() == Some("refusal") {
        let text = if output.is_empty() {
            "stop_reason: refusal".to_owned()
        } else {
            output
        };
        return Err(GatewayError::Refused(text));
    }
    let usage = resp.usage.map_or_else(TokenUsage::default, |u| {
        let prompt_tokens = u
            .input_tokens
            .saturating_add(u.cache_creation_input_tokens)
            .saturating_add(u.cache_read_input_tokens);
        TokenUsage {
            prompt_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: prompt_tokens.saturating_add(u.output_tokens),
            reasoning_tokens: u.output_tokens_details.map_or(0, |d| d.thinking_tokens),
            cached_prompt_tokens: u.cache_read_input_tokens,
            cache_creation_prompt_tokens: u.cache_creation_input_tokens,
        }
    });
    Ok(LlmReply {
        thinking: thinking.join("\n"),
        output,
        finish_reason: resp.stop_reason.as_deref().map(finish_reason),
        tool_calls,
        usage,
        replay,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ResolvedParams {
        ResolvedParams {
            model: "claude-haiku-4-5".to_owned(),
            reasoning_effort: None,
            max_tokens: None,
            thinking_budget: None,
            prompt_cache_key: Some("session-1".to_owned()),
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> RequestedToolCall {
        RequestedToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    fn tools() -> Vec<ToolDef> {
        ["search", "fetch"]
            .iter()
            .map(|name| ToolDef {
                name: (*name).to_owned(),
                description: format!("{name} tool"),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            })
            .collect()
    }

    fn parse(body: serde_json::Value) -> Result<LlmReply, GatewayError> {
        parse_response(serde_json::from_value(body).expect("fixture deserializes"))
    }

    fn cache_breakpoints(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(cache_breakpoints).sum::<usize>()
            }
            serde_json::Value::Array(items) => items.iter().map(cache_breakpoints).sum(),
            _ => 0,
        }
    }

    #[test]
    fn tool_turn_maps_to_tool_use_then_one_user_message_of_results_first() {
        let transcript = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("goal"),
            ChatMessage::assistant_tool_calls(
                "looking",
                &[
                    call("c1", "search", r#"{"queries":["a"]}"#),
                    call("c2", "fetch", r#"{"url":"https://x.test"}"#),
                ],
                &ReplayBlocks::default(),
            ),
            ChatMessage::tool("c1", "hits"),
            ChatMessage::tool("c2", "page"),
        ];
        let body = request_body(&params(), &transcript, None, None, None);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(
            messages.len(),
            3,
            "user goal, assistant, user results: {body}"
        );
        let assistant = &messages[1]["content"];
        assert_eq!(
            assistant[0],
            serde_json::json!({"type": "text", "text": "looking"})
        );
        assert_eq!(assistant[1]["type"], "tool_use");
        assert_eq!(assistant[1]["input"], serde_json::json!({"queries": ["a"]}));
        assert_eq!(assistant[2]["id"], "c2");
        let results = messages[2]["content"].as_array().expect("results");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r["type"] == "tool_result"));
        assert_eq!(results[0]["tool_use_id"], "c1");
        assert_eq!(results[1]["content"], "page");
    }

    #[test]
    fn system_goes_top_level_and_breakpoints_cover_tools_system_and_tail() {
        let transcript = vec![ChatMessage::system("sys"), ChatMessage::user("goal")];
        let body = request_body(
            &params(),
            &transcript,
            Some(&tools()),
            Some(&ToolChoice::Auto),
            None,
        );
        assert_eq!(
            body["system"],
            serde_json::json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}])
        );
        assert!(body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .all(|m| m["role"] != "system"));
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][1]["input_schema"]["type"], "object");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(cache_breakpoints(&body), 3);
        assert_eq!(body["tool_choice"], serde_json::json!({"type": "auto"}));
        // No Messages equivalent: never sent.
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn max_tokens_defaults_and_grows_by_the_thinking_budget() {
        let transcript = [ChatMessage::user("goal")];
        let body = request_body(&params(), &transcript, None, None, None);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert!(body.get("thinking").is_none());

        let mut with_budget = params();
        with_budget.thinking_budget = Some(2048);
        let body = request_body(&with_budget, &transcript, None, None, None);
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS + 2048);
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 2048})
        );

        let mut explicit = with_budget.clone();
        explicit.max_tokens = Some(4096);
        explicit.reasoning_effort = Some(ReasoningEffort::Low);
        let body = request_body(&explicit, &transcript, None, None, None);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["output_config"], serde_json::json!({"effort": "low"}));
    }

    #[test]
    fn extra_body_fills_gaps_but_typed_keys_win() {
        let extra = serde_json::json!({"model": "other", "service_tier": "auto"});
        let body = request_body(
            &params(),
            &[ChatMessage::user("goal")],
            None,
            None,
            Some(&extra),
        );
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["service_tier"], "auto");
    }

    #[test]
    fn response_maps_blocks_stop_reason_and_usage() {
        let reply = parse(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "plan", "signature": "sig-1"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "searching"},
                {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"queries": ["q"]}}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 7,
                "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": 4000,
                "output_tokens_details": {"thinking_tokens": 3}
            }
        }))
        .expect("message parses");
        assert_eq!(reply.output, "searching");
        assert_eq!(reply.thinking, "plan");
        assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(
            reply.tool_calls,
            vec![call("toolu_1", "search", r#"{"queries":["q"]}"#)]
        );
        assert_eq!(reply.replay.blocks.len(), 2);
        assert_eq!(reply.replay.blocks[1]["type"], "redacted_thinking");
        assert_eq!(reply.usage.prompt_tokens, 4110);
        assert_eq!(reply.usage.total_tokens, 4117);
        assert_eq!(reply.usage.cached_prompt_tokens, 4000);
        assert_eq!(reply.usage.cache_creation_prompt_tokens, 100);
        assert_eq!(reply.usage.reasoning_tokens, 3);
    }

    #[test]
    fn replayed_thinking_blocks_come_back_verbatim_before_tool_use() {
        let reply = parse(serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "plan", "signature": "sig-1"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {}}
            ],
            "stop_reason": "tool_use"
        }))
        .expect("message parses");
        let transcript = vec![
            ChatMessage::user("goal"),
            ChatMessage::assistant_tool_calls(&reply.output, &reply.tool_calls, &reply.replay),
            ChatMessage::tool("toolu_1", "hits"),
        ];
        let body = request_body(&params(), &transcript, None, None, None);
        let assistant = &body["messages"][1]["content"];
        assert_eq!(
            assistant[0],
            serde_json::json!({"type": "thinking", "thinking": "plan", "signature": "sig-1"})
        );
        assert_eq!(
            assistant[1],
            serde_json::json!({"type": "redacted_thinking", "data": "opaque"})
        );
        assert_eq!(assistant[2]["type"], "tool_use");
        assert_eq!(assistant.as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn stop_reasons_map_to_finish_reasons() {
        for (stop, finish) in [
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
            ("max_tokens", "length"),
            ("model_context_window_exceeded", "length"),
            ("pause_turn", "pause_turn"),
        ] {
            let reply = parse(serde_json::json!({
                "content": [{"type": "text", "text": "x"}],
                "stop_reason": stop
            }))
            .expect("message parses");
            assert_eq!(reply.finish_reason.as_deref(), Some(finish), "{stop}");
        }
    }

    #[test]
    fn whitespace_only_text_never_reaches_the_wire() {
        // Empty-answer repair path: the model replied "\n\n", the loop keeps
        // it as assistant commentary and appends a user repair nudge.
        let transcript = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("goal"),
            ChatMessage::assistant("\n\n"),
            ChatMessage::user("answer with markdown"),
            ChatMessage::assistant_tool_calls(
                "  ",
                &[call("c1", "search", "{}")],
                &ReplayBlocks::default(),
            ),
            ChatMessage::tool("c1", "hits"),
            ChatMessage::user(" \t"),
        ];
        let body = request_body(&params(), &transcript, None, None, None);
        let messages = body["messages"].as_array().expect("messages");
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(roles, vec!["user", "user", "assistant", "user"], "{body}");
        for message in messages {
            for block in message["content"].as_array().expect("block array") {
                if block["type"] == "text" {
                    let text = block["text"].as_str().unwrap_or_default();
                    assert!(!text.trim().is_empty(), "blank text block sent: {body}");
                }
            }
        }
        assert_eq!(messages[2]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn refusal_and_error_bodies_are_errors() {
        let refused = parse(serde_json::json!({
            "content": [{"type": "text", "text": "I can't help"}],
            "stop_reason": "refusal"
        }))
        .expect_err("refusal must fail");
        assert!(matches!(refused, GatewayError::Refused(text) if text == "I can't help"));

        let error = parse(serde_json::json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        }))
        .expect_err("error body must fail");
        assert!(matches!(error, GatewayError::Parse(text) if text == "Overloaded"));
    }
}
