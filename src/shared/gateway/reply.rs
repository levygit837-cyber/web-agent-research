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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<ToolChoice>,
}

/// One callable function offered to the model. Serializes to the OpenAI
/// `{type: "function", function: {name, description, parameters}}` shape;
/// `parameters` is a real JSON Schema value and is always sent.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// How the model may use the offered tools: automatic, none, or one named
/// function (`{type: "function", function: {name}}` on the wire).
#[derive(Debug, Clone)]
pub enum ToolChoice {
    Auto,
    None,
    Named(String),
}

impl serde::Serialize for ToolDef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut outer = serializer.serialize_map(Some(2))?;
        outer.serialize_entry("type", "function")?;
        outer.serialize_entry(
            "function",
            &serde_json::json!({
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }),
        )?;
        outer.end()
    }
}

impl serde::Serialize for ToolChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::None => serializer.serialize_str("none"),
            Self::Named(name) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "function")?;
                map.serialize_entry("function", &serde_json::json!({"name": name}))?;
                map.end()
            }
        }
    }
}

impl ChatRequest {
    pub(crate) fn from_resolved(
        params: &super::config::ResolvedParams,
        messages: Vec<ChatMessage>,
    ) -> Self {
        Self::from_resolved_with_tools(params, messages, None, None)
    }

    pub(crate) fn from_resolved_with_tools(
        params: &super::config::ResolvedParams,
        messages: Vec<ChatMessage>,
        tools: Option<Vec<ToolDef>>,
        tool_choice: Option<ToolChoice>,
    ) -> Self {
        Self {
            model: params.model.clone(),
            messages,
            reasoning_effort: params.reasoning_effort.clone(),
            max_completion_tokens: params.max_tokens,
            tools,
            tool_choice,
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
    /// Wire `finish_reason` verbatim ("stop", "length", "tool_calls", ...). None if absent.
    pub finish_reason: Option<String>,
    /// Requested tool calls, in wire order. Empty when the model answered
    /// with text; existing text-only consumers ignore this field.
    pub tool_calls: Vec<RequestedToolCall>,
    pub usage: TokenUsage,
}

/// One function call requested by the model. The gateway never executes it;
/// `arguments` is the raw JSON string; [`RequestedToolCall::parsed_arguments`]
/// parses it on demand.
#[derive(Debug, Clone, Default)]
pub struct RequestedToolCall {
    /// Wire `id` (`""` when absent).
    pub id: String,
    /// Requested function name (`""` when absent, but still returned).
    pub name: String,
    /// Raw arguments JSON string (`"{}"` when absent).
    pub arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// From nested `completion_tokens_details`; 0 if absent.
    pub reasoning_tokens: u64,
}

impl RequestedToolCall {
    /// Parse the raw `arguments` string as JSON. Invalid JSON is a
    /// non-retryable [`GatewayError::Parse`].
    pub fn parsed_arguments(&self) -> Result<serde_json::Value, GatewayError> {
        serde_json::from_str(&self.arguments).map_err(|err| GatewayError::Parse(err.to_string()))
    }
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
        tool_calls,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::gateway::config::ResolvedParams;

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
    fn request_omits_tools_when_none() {
        let params = ResolvedParams {
            model: "m".to_owned(),
            reasoning_effort: None,
            max_tokens: None,
        };
        let body = serde_json::to_value(ChatRequest::from_resolved_with_tools(
            &params,
            vec![],
            None,
            None,
        ))
        .expect("request must serialize");
        let obj = body.as_object().expect("body is an object");
        assert!(
            !obj.contains_key("tools"),
            "absent tools must be omitted, got {body}"
        );
        assert!(
            !obj.contains_key("tool_choice"),
            "absent tool_choice must be omitted, got {body}"
        );
    }

    #[test]
    fn request_serializes_tools_and_auto_choice() {
        let params = ResolvedParams {
            model: "m".to_owned(),
            reasoning_effort: None,
            max_tokens: None,
        };
        let tools = vec![ToolDef {
            name: "get_time".to_owned(),
            description: "Returns the current time.".to_owned(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let body = serde_json::to_value(ChatRequest::from_resolved_with_tools(
            &params,
            vec![],
            Some(tools),
            Some(ToolChoice::Auto),
        ))
        .expect("request must serialize");
        assert_eq!(
            body["tools"],
            serde_json::json!([{
                "type": "function",
                "function": {
                    "name": "get_time",
                    "description": "Returns the current time.",
                    "parameters": {"type": "object", "properties": {}}
                }
            }]),
            "tools must use the OpenAI function shape, got {body}"
        );
        assert_eq!(body["tool_choice"], serde_json::json!("auto"));
        assert_eq!(
            serde_json::to_value(ToolChoice::None).expect("choice must serialize"),
            serde_json::json!("none")
        );
    }

    #[test]
    fn request_serializes_named_choice() {
        let params = ResolvedParams {
            model: "m".to_owned(),
            reasoning_effort: None,
            max_tokens: None,
        };
        let body = serde_json::to_value(ChatRequest::from_resolved_with_tools(
            &params,
            vec![],
            Some(vec![]),
            Some(ToolChoice::Named("get_time".to_owned())),
        ))
        .expect("request must serialize");
        assert_eq!(
            body["tool_choice"],
            serde_json::json!({"type": "function", "function": {"name": "get_time"}}),
            "named choice must use the OpenAI function shape, got {body}"
        );
    }
}
