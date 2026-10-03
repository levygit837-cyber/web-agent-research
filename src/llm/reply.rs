//! Format-neutral gateway reply contract: both wire formats
//! (`llm::response`'s OpenAI parser, `llm::anthropic`'s Messages parser)
//! parse into [`LlmReply`].
//!
//! Split out per ADR-0009: `llm::request` owns the request-building side,
//! `llm::response` owns the OpenAI response-parsing side; this file keeps
//! only the shape both sides agree on.

use super::client::GatewayError;
use super::request::ReplayBlocks;

/// One completed (non-streaming) turn from the gateway, in a
/// format-neutral shape: both wire formats parse into it.
#[derive(Debug, Clone, Default)]
pub struct LlmReply {
    /// Concatenated thinking, per precedence in `response::parse_reply`.
    /// May be empty.
    pub thinking: String,
    /// Assistant message text. Never None: null content becomes "".
    pub output: String,
    /// Finish reason in the OpenAI vocabulary ("stop", "length",
    /// "tool_calls", ...); Anthropic `stop_reason` values are mapped onto
    /// it. None if absent.
    pub finish_reason: Option<String>,
    /// Requested tool calls, in wire order. Empty when the model answered
    /// with text; existing text-only consumers ignore this field.
    pub tool_calls: Vec<RequestedToolCall>,
    pub usage: TokenUsage,
    /// Blocks the next request must echo on this assistant turn; pass to
    /// [`super::request::ChatMessage::assistant_tool_calls`]. Empty for
    /// OpenAI replies.
    pub replay: ReplayBlocks,
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
    /// Total input tokens, cached ones included (Anthropic:
    /// `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`).
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// From nested `completion_tokens_details` (OpenAI) or
    /// `output_tokens_details.thinking_tokens` (Anthropic); 0 if absent.
    pub reasoning_tokens: u64,
    /// Cached input tokens read: max of OpenAI's
    /// `usage.prompt_tokens_details.cached_tokens` and the Anthropic/LiteLLM
    /// top-level `usage.cache_read_input_tokens`; 0 if neither is present.
    pub cached_prompt_tokens: u64,
    /// Input tokens written to the prompt cache this turn (Anthropic
    /// `usage.cache_creation_input_tokens`); 0 when the provider does not
    /// report cache writes.
    pub cache_creation_prompt_tokens: u64,
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
        self.cache_creation_prompt_tokens = self
            .cache_creation_prompt_tokens
            .saturating_add(other.cache_creation_prompt_tokens);
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

#[cfg(test)]
mod tests {
    use super::*;

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
                cache_creation_prompt_tokens: 1,
            });
        }
        assert_eq!(total.prompt_tokens, 30);
        assert_eq!(total.completion_tokens, 15);
        assert_eq!(total.total_tokens, 45);
        assert_eq!(total.reasoning_tokens, 6);
        assert_eq!(total.cached_prompt_tokens, 12);
        assert_eq!(total.cache_creation_prompt_tokens, 3);
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
}
