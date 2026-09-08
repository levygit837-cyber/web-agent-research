//! Gateway wire types: OpenAI-compatible chat bodies and the parsed turn.
//!
//! Tolerance-first: every response field carries `#[serde(default)]`, and
//! non-optional fields additionally treat explicit wire `null` as missing, so
//! a new thinking shape degrades to empty `thinking`, never to a parse failure.

use serde::{Deserialize, Serialize};

use super::client::GatewayError;

/// One message in a chat request. The agent loop (issue #12) owns roles/content.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Non-streaming `/chat/completions` request body.
#[derive(Debug, Serialize)]
pub(crate) struct ChatRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_completion_tokens: Option<u32>,
}

impl ChatRequest {
    pub(crate) fn from_resolved(
        params: &super::config::ResolvedParams,
        messages: Vec<ChatMessage>,
    ) -> Self {
        Self {
            model: params.model.clone(),
            messages,
            reasoning_effort: params.reasoning_effort.clone(),
            max_completion_tokens: params.max_tokens,
        }
    }
}

/// Explicit wire `null` means "absent" for every non-optional response field.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
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
    // Present on the wire; denied, never executed (see `parse_reply`).
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

// Wire shape only: parsed solely to deny tool calls, never executed.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ToolCall {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default, rename = "type")]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) function: Option<ToolFunction>,
}

// Wire shape only: see `ToolCall`.
#[allow(dead_code)]
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
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CompletionDetails {
    #[serde(default, deserialize_with = "null_to_default")]
    pub(crate) reasoning_tokens: u64,
}

/// One completed (non-streaming) turn from the gateway.
#[derive(Debug, Clone, Default)]
pub struct LlmReply {
    /// Concatenated thinking, per precedence in `parse_reply`. May be empty.
    pub thinking: String,
    /// Assistant message text. Never None: null content becomes "".
    pub output: String,
    /// Wire `finish_reason` verbatim ("stop", "length", ...). None if absent.
    pub finish_reason: Option<String>,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// From nested `completion_tokens_details`; 0 if absent.
    pub reasoning_tokens: u64,
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
    if !msg.tool_calls.is_empty() {
        return Err(GatewayError::ToolCallsUnsupported);
    }
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
    let usage = resp.usage.map_or_else(TokenUsage::default, |u| TokenUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        reasoning_tokens: u
            .completion_tokens_details
            .map_or(0, |d| d.reasoning_tokens),
    });
    Ok(LlmReply {
        thinking: parts.join("\n"),
        output: msg.content.unwrap_or_default(),
        finish_reason: choice.finish_reason,
        usage,
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
    fn tool_calls_denied() {
        let err = parse(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "answer",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "search", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
        )
        .expect_err("tool calls must be denied");
        assert!(matches!(err, GatewayError::ToolCallsUnsupported));
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
}
