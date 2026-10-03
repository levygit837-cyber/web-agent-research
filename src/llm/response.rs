//! Gateway response types: the native OpenAI `/chat/completions` response
//! shape and the parser that turns it into the format-neutral
//! [`super::reply::LlmReply`].
//!
//! Split out of `llm::reply` per ADR-0009: this file owns everything a
//! caller parses a response *into*; `llm::request` owns the request side;
//! `llm::reply` keeps only the format-neutral contract.
//!
//! Tolerance-first: every response field carries `#[serde(default)]`, and
//! non-optional fields additionally treat explicit wire `null` as missing, so
//! a new thinking shape degrades to empty `thinking`, never to a parse failure.

use serde::Deserialize;

use super::client::GatewayError;
use super::reply::{LlmReply, RequestedToolCall, TokenUsage};
use super::request::ReplayBlocks;

/// Explicit wire `null` means "absent" for every non-optional response field.
pub(crate) fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatResponse {
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) choices: Vec<Choice>,
    #[serde(default)]
    pub(crate) usage: Option<ResponseUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Choice {
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) message: ResponseMessage,
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResponseMessage {
    // Wire shape only; the model's own role label is never consumed.
    #[allow(dead_code)]
    #[serde(default)]
    pub(crate) role: Option<String>,
    // Nullable content: null -> None -> "" (see `parse_reply`).
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) refusal: Option<String>,
    // Shape (a): glm-5p2 style.
    #[serde(default)]
    pub(crate) reasoning_content: Option<String>,
    // Shape (b): mimo/opencode style scalar ...
    #[serde(default)]
    pub(crate) reasoning: Option<String>,
    // ... plus detail blocks.
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) reasoning_details: Vec<ReasoningDetail>,
    // Requested function calls; collected into `LlmReply::tool_calls`.
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ReasoningDetail {
    // Wire shape only; block kind is never consumed, only `text`.
    #[allow(dead_code)]
    #[serde(default, rename = "type")]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ToolCall {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) function: Option<ToolFunction>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ToolFunction {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) arguments: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResponseUsage {
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) prompt_tokens: u64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) completion_tokens: u64,
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) total_tokens: u64,
    #[serde(default)]
    pub(crate) completion_tokens_details: Option<CompletionDetails>,
    /// OpenAI shape: `usage.prompt_tokens_details.cached_tokens`.
    #[serde(default)]
    pub(crate) prompt_tokens_details: Option<PromptTokensDetails>,
    /// Anthropic/LiteLLM shape: top-level `usage.cache_read_input_tokens`.
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CompletionDetails {
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) reasoning_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) cached_tokens: u64,
}

pub(crate) fn parse_reply(resp: ChatResponse) -> Result<LlmReply, GatewayError> {
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or(GatewayError::EmptyChoices)?;
    let msg = choice.message;
    if let Some(text) = msg.refusal.as_deref().filter(|s| !s.is_empty()) {
        return Err(GatewayError::Refused(text.to_owned()));
    }
    let tool_calls = msg
        .tool_calls
        .iter()
        .map(|call| RequestedToolCall {
            id: call.id.clone().unwrap_or_default(),
            name: call
                .function
                .as_ref()
                .and_then(|f| f.name.clone())
                .unwrap_or_default(),
            arguments: call
                .function
                .as_ref()
                .and_then(|f| f.arguments.clone())
                .unwrap_or_else(|| "{}".to_owned()),
        })
        .collect();
    let mut parts: Vec<&str> = Vec::new();
    for part in [msg.reasoning_content.as_deref(), msg.reasoning.as_deref()] {
        if let Some(text) = part.filter(|s| !s.is_empty()) {
            parts.push(text);
        }
    }
    for detail in &msg.reasoning_details {
        if let Some(text) = detail.text.as_deref().filter(|s| !s.is_empty()) {
            parts.push(text);
        }
    }
    let usage = resp.usage.map_or_else(TokenUsage::default, |u| {
        let openai_cached = u
            .prompt_tokens_details
            .as_ref()
            .map_or(0, |d| d.cached_tokens);
        let anthropic_cached = u.cache_read_input_tokens.unwrap_or(0);
        TokenUsage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            reasoning_tokens: u
                .completion_tokens_details
                .map_or(0, |d| d.reasoning_tokens),
            cached_prompt_tokens: openai_cached.max(anthropic_cached),
            cache_creation_prompt_tokens: 0,
        }
    });
    Ok(LlmReply {
        thinking: parts.join("\n"),
        output: msg.content.unwrap_or_default(),
        finish_reason: choice.finish_reason,
        tool_calls,
        usage,
        replay: ReplayBlocks::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(fixture: &str) -> Result<LlmReply, GatewayError> {
        let resp: ChatResponse = serde_json::from_str(fixture).expect("fixture must deserialize");
        parse_reply(resp)
    }

    #[test]
    fn shape_a_reasoning_content() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "done",
                        "reasoning_content": "plan the search first",
                        "reasoning_details": null
                    },
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            }"#,
        )
        .expect("shape (a) must parse");
        assert_eq!(reply.thinking, "plan the search first");
        assert_eq!(reply.output, "done");
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn shape_b_reasoning_blocks() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "answer",
                        "reasoning": "scalar thought",
                        "reasoning_details": [
                            {"type": "reasoning.text", "text": "block one"},
                            {"type": "reasoning.text", "text": "block two"}
                        ]
                    },
                    "finish_reason": "stop"
                }]
            }"#,
        )
        .expect("shape (b) must parse");
        assert_eq!(reply.thinking, "scalar thought\nblock one\nblock two");
        assert_eq!(reply.output, "answer");
    }

    #[test]
    fn both_shapes_concatenate() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "answer",
                        "reasoning_content": "a-think",
                        "reasoning": "b-think",
                        "reasoning_details": [
                            {"type": "reasoning.text", "text": "d1"},
                            {"type": "reasoning.text", "text": ""}
                        ]
                    },
                    "finish_reason": "stop"
                }]
            }"#,
        )
        .expect("both shapes must parse");
        assert_eq!(reply.thinking, "a-think\nb-think\nd1");
    }

    #[test]
    fn null_content_is_empty_output() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {"role": "assistant", "content": null},
                    "finish_reason": "stop"
                }]
            }"#,
        )
        .expect("null content must parse");
        assert_eq!(reply.output, "");
        assert_eq!(reply.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn refusal_wins() {
        let err = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "partial",
                        "refusal": "I cannot help with that"
                    },
                    "finish_reason": "stop"
                }]
            }"#,
        )
        .expect_err("refusal must be an error");
        assert!(matches!(err, GatewayError::Refused(text) if text == "I cannot help with that"));
    }

    #[test]
    fn tool_calls_returned() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "reasoning_content": "calling get_time",
                        "tool_calls": [{
                            "id": "chatcmpl-tool-abc123",
                            "type": "function",
                            "function": {"name": "get_time", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
        )
        .expect("tool calls must parse");
        assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(reply.output, "");
        assert_eq!(reply.thinking, "calling get_time");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "chatcmpl-tool-abc123");
        assert_eq!(reply.tool_calls[0].name, "get_time");
        assert_eq!(reply.tool_calls[0].arguments, "{}");
        assert_eq!(
            reply.tool_calls[0]
                .parsed_arguments()
                .expect("arguments must parse"),
            serde_json::json!({})
        );
    }

    #[test]
    fn refusal_wins_over_tool_calls() {
        let err = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "refusal": "I cannot help with that",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "search", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": "stop"
                }]
            }"#,
        )
        .expect_err("refusal must win over tool calls");
        assert!(matches!(err, GatewayError::Refused(text) if text == "I cannot help with that"));
    }

    #[test]
    fn empty_tool_calls_is_text_answer() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {"role": "assistant", "content": "answer"},
                    "finish_reason": "stop"
                }]
            }"#,
        )
        .expect("text answer must parse");
        assert!(reply.tool_calls.is_empty());
        assert_eq!(reply.output, "answer");
    }

    #[test]
    fn invalid_tool_arguments_is_parse_error() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "search", "arguments": "{bad json"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
        )
        .expect("invalid arguments still parse at the wire level");
        assert_eq!(reply.tool_calls.len(), 1);
        let err = reply.tool_calls[0]
            .parsed_arguments()
            .expect_err("invalid arguments JSON must fail");
        assert!(matches!(err, GatewayError::Parse(_)));
    }

    #[test]
    fn missing_tool_fields_default() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{"function": {}}]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
        )
        .expect("missing tool fields must parse");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "");
        assert_eq!(reply.tool_calls[0].name, "");
        assert_eq!(reply.tool_calls[0].arguments, "{}");
    }

    #[test]
    fn empty_choices() {
        let err = parse(r#"{"choices": [], "usage": null}"#).expect_err("empty choices must fail");
        assert!(matches!(err, GatewayError::EmptyChoices));
    }

    #[test]
    fn usage_nesting() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {"role": "assistant", "content": "answer"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 7,
                    "completion_tokens": 11,
                    "total_tokens": 18,
                    "completion_tokens_details": {"reasoning_tokens": 42}
                }
            }"#,
        )
        .expect("usage must parse");
        assert_eq!(reply.usage.prompt_tokens, 7);
        assert_eq!(reply.usage.completion_tokens, 11);
        assert_eq!(reply.usage.total_tokens, 18);
        assert_eq!(reply.usage.reasoning_tokens, 42);

        let bare = parse(r#"{"choices": [{"message": {"role": "assistant", "content": "x"}}]}"#)
            .expect("absent usage must parse");
        assert_eq!(bare.usage.prompt_tokens, 0);
        assert_eq!(bare.usage.completion_tokens, 0);
        assert_eq!(bare.usage.total_tokens, 0);
        assert_eq!(bare.usage.reasoning_tokens, 0);
    }

    #[test]
    fn openai_cached_tokens_shape_populates_cached_prompt_tokens() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {"role": "assistant", "content": "answer"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "total_tokens": 120,
                    "prompt_tokens_details": {"cached_tokens": 80}
                }
            }"#,
        )
        .expect("OpenAI cached-tokens shape must parse");
        assert_eq!(reply.usage.cached_prompt_tokens, 80);
    }

    #[test]
    fn anthropic_cache_read_input_tokens_shape_populates_cached_prompt_tokens() {
        let reply = parse(
            r#"{
                "choices": [{
                    "message": {"role": "assistant", "content": "answer"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "total_tokens": 120,
                    "cache_read_input_tokens": 55
                }
            }"#,
        )
        .expect("Anthropic/LiteLLM cache_read_input_tokens shape must parse");
        assert_eq!(reply.usage.cached_prompt_tokens, 55);
    }

    #[test]
    fn both_cache_shapes_present_takes_the_max() {
        let lower_openai = parse(
            r#"{
                "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 100, "completion_tokens": 1, "total_tokens": 101,
                    "prompt_tokens_details": {"cached_tokens": 30},
                    "cache_read_input_tokens": 90
                }
            }"#,
        )
        .expect("both shapes must parse");
        assert_eq!(lower_openai.usage.cached_prompt_tokens, 90);

        let higher_openai = parse(
            r#"{
                "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 100, "completion_tokens": 1, "total_tokens": 101,
                    "prompt_tokens_details": {"cached_tokens": 90},
                    "cache_read_input_tokens": 30
                }
            }"#,
        )
        .expect("both shapes must parse");
        assert_eq!(higher_openai.usage.cached_prompt_tokens, 90);
    }

    #[test]
    fn absent_cache_fields_default_to_zero() {
        let reply = parse(
            r#"{
                "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
            }"#,
        )
        .expect("usage without cache fields must parse");
        assert_eq!(reply.usage.cached_prompt_tokens, 0);
    }
}
