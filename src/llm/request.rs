//! Gateway request types: the format-neutral chat transcript and the
//! native OpenAI `/chat/completions` request body built from it.
//!
//! Split out of `llm::reply` per ADR-0009: this file owns everything a
//! caller builds a request *from*; `llm::response` owns everything a
//! caller parses a response *into*; `llm::reply` keeps only the
//! format-neutral contract shared by both wire formats.

use serde::Serialize;

use super::config::ReasoningEffort;
use super::reply::RequestedToolCall;

/// One message in a chat request, matching the native OpenAI wire shapes:
/// `system`/`user`/`assistant` carry text `content`; an `assistant` turn
/// that calls tools carries `tool_calls` (`content` is wire `null` when the
/// model sent no accompanying text); a `tool` reply carries `tool_call_id`
/// plus `content`. Built only through the constructors below; direct field
/// construction stays out of `research::agent_loop`. The Anthropic wire
/// format (`llm::anthropic`) maps the same messages onto content blocks.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<RequestedToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Provider blocks that must travel back verbatim on this assistant
    /// turn (Anthropic `thinking`/`redacted_thinking`). Never on the
    /// OpenAI wire.
    #[serde(skip)]
    pub(crate) replay: ReplayBlocks,
}

/// Opaque provider blocks from one assistant turn that the next request
/// must echo unmodified and in order: Anthropic requires the `thinking`
/// and `redacted_thinking` blocks (with their `signature`/`data`) of a
/// tool-use turn to come back byte-identical. Empty for OpenAI replies.
/// `research` stores and forwards it without looking inside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayBlocks {
    pub(crate) blocks: Vec<serde_json::Value>,
    /// Total chars of the blocks' string fields, computed once at parse
    /// time so context budgeting never re-serializes them.
    pub(crate) chars: usize,
}

impl ReplayBlocks {
    /// Approximate size for char-based context budgeting.
    pub fn chars(&self) -> usize {
        self.chars
    }
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
            replay: ReplayBlocks::default(),
        }
    }

    /// Assistant turn that requests tool calls, wire `tool_calls` exactly as
    /// given (ids included, in call order). `text` is the model's own
    /// commentary alongside the calls, if any; empty maps to wire
    /// `content: null`, the native shape for a pure tool-call turn.
    /// `replay` is the turn's [`super::reply::LlmReply::replay`], echoed back verbatim.
    pub fn assistant_tool_calls(
        text: &str,
        calls: &[RequestedToolCall],
        replay: &ReplayBlocks,
    ) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: (!text.is_empty()).then(|| text.to_owned()),
            tool_calls: Some(calls.to_vec()),
            tool_call_id: None,
            replay: replay.clone(),
        }
    }

    /// One tool result, paired to the requesting call's `id`.
    pub fn tool(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
            replay: ReplayBlocks::default(),
        }
    }

    /// Text content, empty when the wire value is `null` (a pure tool-call
    /// turn or, for other roles, never in practice).
    pub fn content_text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}

/// `thinking` request param: native on the Anthropic Messages API, also
/// accepted by the Anthropic OpenAI-compat layer and DeepSeek. Internally
/// tagged on `type` so `Enabled` serializes flat as
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
    pub(crate) fn from_budget(budget: u32) -> Self {
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
    /// Always `false`: the client reads one complete JSON body. Typed, so
    /// it wins over `GATEWAY_EXTRA_BODY` and streaming cannot be enabled.
    pub(crate) stream: bool,
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
    #[cfg(test)]
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
            stream: false,
        }
    }

    /// Final wire body: the typed shape merged over `extra` (see
    /// [`merge_extra_body`]).
    pub(crate) fn to_wire_value(&self, extra: Option<&serde_json::Value>) -> serde_json::Value {
        let typed = serde_json::to_value(self).expect("ChatRequest always serializes");
        merge_extra_body(typed, extra)
    }
}

/// OpenAI SDK `extra_body` semantics, shared by both wire formats: start
/// from `extra`, then overlay every key of the `typed` body, so `model`,
/// `messages`, `tools`, `tool_choice`, and every set typed param win on
/// collision while gateway-specific keys absent from the typed shape
/// (`reasoning`, `prompt_cache_retention`, `verbosity`, ...) pass through
/// untouched. A non-object `extra` (shouldn't happen —
/// `GatewayConfig::from_env` rejects it) is ignored rather than corrupting
/// the body.
pub(crate) fn merge_extra_body(
    typed: serde_json::Value,
    extra: Option<&serde_json::Value>,
) -> serde_json::Value {
    let Some(extra) = extra.filter(|v| v.is_object()) else {
        return typed;
    };
    let mut merged = extra.clone();
    let base = merged
        .as_object_mut()
        .expect("filtered to a JSON object above");
    if let serde_json::Value::Object(over) = typed {
        for (key, value) in over {
            base.insert(key, value);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::config::ResolvedParams;

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
