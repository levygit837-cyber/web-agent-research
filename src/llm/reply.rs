//! Gateway wire types: OpenAI-compatible chat bodies and the parsed turn.
//!
//! Tolerance-first: every response field carries `#[serde(default)]`, and
//! non-optional fields additionally treat explicit wire `null` as missing, so
//! a new thinking shape degrades to empty `thinking`, never to a parse failure.

use serde::{Deserialize, Serialize};

use super::client::GatewayError;
use super::config::ReasoningEffort;

/// One message in a chat request, matching the native OpenAI wire shapes:
/// `system`/`user`/`assistant` carry text `content`; an `assistant` turn
/// that calls tools carries `tool_calls` (`content` is wire `null` when the
/// model sent no accompanying text); a `tool` reply carries `tool_call_id`
/// plus `content`. Built only through the constructors below; direct field
/// construction stays out of `research::agent_loop`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<RequestedToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text("system", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text("user", content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text("assistant", content)
    }

    fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_owned(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Assistant turn that requests tool calls, wire `tool_calls` exactly as
    /// given (ids included, in call order). `text` is the model's own
    /// commentary alongside the calls, if any; empty maps to wire
    /// `content: null`, the native shape for a pure tool-call turn.
    pub fn assistant_tool_calls(text: &str, calls: &[RequestedToolCall]) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: (!text.is_empty()).then(|| text.to_owned()),
            tool_calls: Some(calls.to_vec()),
            tool_call_id: None,
        }
    }

    /// One tool result, paired to the requesting call's `id`.
    pub fn tool(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
        }
    }

    /// Text content, empty when the wire value is `null` (a pure tool-call
    /// turn or, for other roles, never in practice).
    pub fn content_text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}

/// `thinking` request param (Anthropic OpenAI-compat layer, DeepSeek).
/// Internally tagged on `type` so `Enabled` serializes flat as
/// `{"type": "enabled", "budget_tokens": n}` and `Disabled` as
/// `{"type": "disabled"}` (no `budget_tokens` key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum ThinkingParam {
    Enabled { budget_tokens: u32 },
    Disabled,
}

impl ThinkingParam {
    /// `0` disables thinking; any other value enables it with that budget.
    fn from_budget(budget: u32) -> Self {
        if budget == 0 {
            Self::Disabled
        } else {
            Self::Enabled {
                budget_tokens: budget,
            }
        }
    }
}

/// Non-streaming `/chat/completions` request body.
#[derive(Debug, Serialize)]
pub(crate) struct ChatRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_key: Option<String>,
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
            reasoning_effort: params.reasoning_effort,
            max_completion_tokens: params.max_tokens,
            thinking: params.thinking_budget.map(ThinkingParam::from_budget),
            prompt_cache_key: params.prompt_cache_key.clone(),
            tools,
            tool_choice,
        }
    }

    /// Final wire body. With no `extra_body`, just the typed shape. With
    /// one, OpenAI SDK `extra_body` semantics: start from `extra`, then
    /// overlay every key this struct actually serializes, so `model`,
    /// `messages`, `tools`, `tool_choice`, and every set typed param win on
    /// collision while gateway-specific keys absent from the typed shape
    /// (`reasoning`, `prompt_cache_retention`, `verbosity`, ...) pass
    /// through untouched. A non-object `extra` (shouldn't happen —
    /// `GatewayConfig::from_env` rejects it) is ignored rather than
    /// corrupting the body.
    pub(crate) fn to_wire_value(&self, extra: Option<&serde_json::Value>) -> serde_json::Value {
        let typed = serde_json::to_value(self).expect("ChatRequest always serializes");
        let Some(extra) = extra.filter(|v| v.is_object()) else {
            return typed;
        };
        let mut merged = extra.clone();
        let base = merged
            .as_object_mut()
            .expect("filtered to a JSON object above");
        if let Some(over) = typed.as_object() {
            for (key, value) in over {
                base.insert(key.clone(), value.clone());
            }
        }
        merged
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// Cached input tokens: max of OpenAI's
    /// `usage.prompt_tokens_details.cached_tokens` and the Anthropic/LiteLLM
    /// top-level `usage.cache_read_input_tokens`; 0 if neither is present.
    pub cached_prompt_tokens: u64,
}

impl TokenUsage {
    /// Saturating per-field accumulation. The agent loop calls this once per
    /// turn to fold that turn's [`LlmReply::usage`] into the run total.
    pub fn add(&mut self, other: &TokenUsage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.cached_prompt_tokens = self
            .cached_prompt_tokens
            .saturating_add(other.cached_prompt_tokens);
    }
}

impl RequestedToolCall {
    /// Parse the raw `arguments` string as JSON. Invalid JSON is a
    /// non-retryable [`GatewayError::Parse`].
    pub fn parsed_arguments(&self) -> Result<serde_json::Value, GatewayError> {
        serde_json::from_str(&self.arguments).map_err(|err| GatewayError::Parse(err.to_string()))
    }
}

/// Serializes to the OpenAI request shape:
/// `{id, type: "function", function: {name, arguments}}`. `arguments`
/// travels as the raw JSON string, matching what the model sent and what
/// the wire expects back verbatim.
impl serde::Serialize for RequestedToolCall {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut outer = serializer.serialize_map(Some(3))?;
        outer.serialize_entry("id", &self.id)?;
        outer.serialize_entry("type", "function")?;
        outer.serialize_entry(
            "function",
            &serde_json::json!({
                "name": self.name,
                "arguments": self.arguments,
            }),
        )?;
        outer.end()
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
        }
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
    use crate::llm::config::ResolvedParams;

    fn parse(fixture: &str) -> Result<LlmReply, GatewayError> {
        let resp: ChatResponse = serde_json::from_str(fixture).expect("fixture must deserialize");
        parse_reply(resp)
    }

    fn resolved(model: &str) -> ResolvedParams {
        ResolvedParams {
            model: model.to_owned(),
            reasoning_effort: None,
            max_tokens: None,
            thinking_budget: None,
            prompt_cache_key: None,
        }
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

    #[test]
    fn token_usage_add_sums_saturating_across_turns() {
        let mut total = TokenUsage::default();
        for _ in 0..3 {
            total.add(&TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                reasoning_tokens: 2,
                cached_prompt_tokens: 4,
            });
        }
        assert_eq!(total.prompt_tokens, 30);
        assert_eq!(total.completion_tokens, 15);
        assert_eq!(total.total_tokens, 45);
        assert_eq!(total.reasoning_tokens, 6);
        assert_eq!(total.cached_prompt_tokens, 12);
    }

    #[test]
    fn token_usage_add_saturates_at_u64_max() {
        let mut total = TokenUsage {
            prompt_tokens: u64::MAX - 3,
            cached_prompt_tokens: u64::MAX - 1,
            ..TokenUsage::default()
        };
        total.add(&TokenUsage {
            prompt_tokens: 10,
            cached_prompt_tokens: 10,
            ..TokenUsage::default()
        });
        assert_eq!(total.prompt_tokens, u64::MAX);
        assert_eq!(total.cached_prompt_tokens, u64::MAX);
    }

    #[test]
    fn request_omits_tools_when_none() {
        let params = resolved("m");
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
        let params = resolved("m");
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
        let params = resolved("m");
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

    #[test]
    fn request_serializes_reasoning_effort_lowercase() {
        let mut params = resolved("m");
        params.reasoning_effort = Some(ReasoningEffort::Xhigh);
        let body = serde_json::to_value(ChatRequest::from_resolved(&params, vec![]))
            .expect("request must serialize");
        assert_eq!(body["reasoning_effort"], serde_json::json!("xhigh"));
    }

    #[test]
    fn request_thinking_enabled_shape_for_nonzero_budget() {
        let mut params = resolved("m");
        params.thinking_budget = Some(2048);
        let body = serde_json::to_value(ChatRequest::from_resolved(&params, vec![]))
            .expect("request must serialize");
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 2048})
        );
    }

    #[test]
    fn request_thinking_disabled_shape_for_zero_budget() {
        let mut params = resolved("m");
        params.thinking_budget = Some(0);
        let body = serde_json::to_value(ChatRequest::from_resolved(&params, vec![]))
            .expect("request must serialize");
        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
    }

    #[test]
    fn request_prompt_cache_key_present_when_set() {
        let mut params = resolved("m");
        params.prompt_cache_key = Some("session-123".to_owned());
        let body = serde_json::to_value(ChatRequest::from_resolved(&params, vec![]))
            .expect("request must serialize");
        assert_eq!(body["prompt_cache_key"], serde_json::json!("session-123"));
    }

    #[test]
    fn to_wire_value_without_extra_body_is_the_typed_shape() {
        let params = resolved("m");
        let request = ChatRequest::from_resolved(&params, vec![]);
        let typed = serde_json::to_value(&request).expect("request must serialize");
        assert_eq!(request.to_wire_value(None), typed);
    }

    #[test]
    fn to_wire_value_merges_extra_body_gateway_specific_keys() {
        let params = resolved("m");
        let request = ChatRequest::from_resolved(&params, vec![]);
        let extra = serde_json::json!({
            "reasoning": {"effort": "high"},
            "verbosity": "low",
            "prompt_cache_retention": "24h"
        });
        let merged = request.to_wire_value(Some(&extra));
        assert_eq!(merged["reasoning"], serde_json::json!({"effort": "high"}));
        assert_eq!(merged["verbosity"], serde_json::json!("low"));
        assert_eq!(merged["prompt_cache_retention"], serde_json::json!("24h"));
        assert_eq!(merged["model"], serde_json::json!("m"));
    }

    #[test]
    fn to_wire_value_typed_fields_win_on_collision() {
        let params = resolved("real-model");
        let request = ChatRequest::from_resolved(&params, vec![]);
        let extra = serde_json::json!({
            "model": "should-be-overwritten",
            "verbosity": "low"
        });
        let merged = request.to_wire_value(Some(&extra));
        assert_eq!(
            merged["model"],
            serde_json::json!("real-model"),
            "always-serialized typed model must win over extra_body's model, got {merged}"
        );
        assert_eq!(merged["verbosity"], serde_json::json!("low"));
    }

    #[test]
    fn to_wire_value_extra_body_fills_gap_when_typed_field_unset() {
        // `max_tokens: None` means `max_completion_tokens` never serializes
        // (`skip_serializing_if`), so it is absent from `typed`, not merely
        // zero — there is no collision, and extra_body's value for that key
        // passes through untouched, same as any other gateway-specific key.
        let params = resolved("m");
        let request = ChatRequest::from_resolved(&params, vec![]);
        let extra = serde_json::json!({"max_completion_tokens": 999});
        let merged = request.to_wire_value(Some(&extra));
        assert_eq!(merged["max_completion_tokens"], serde_json::json!(999));
    }

    #[test]
    fn to_wire_value_typed_max_tokens_wins_over_extra_body_when_both_set() {
        let mut params = resolved("m");
        params.max_tokens = Some(50);
        let request = ChatRequest::from_resolved(&params, vec![]);
        let extra = serde_json::json!({"max_completion_tokens": 999});
        let merged = request.to_wire_value(Some(&extra));
        assert_eq!(merged["max_completion_tokens"], serde_json::json!(50));
    }

    #[test]
    fn to_wire_value_ignores_non_object_extra() {
        let params = resolved("m");
        let request = ChatRequest::from_resolved(&params, vec![]);
        let typed = serde_json::to_value(&request).expect("request must serialize");
        let extra = serde_json::json!([1, 2, 3]);
        assert_eq!(request.to_wire_value(Some(&extra)), typed);
    }
}
