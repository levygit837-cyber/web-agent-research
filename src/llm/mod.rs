//! LLM gateway: the model behind `research::agent_loop`. Never imports `web`.
//!
//! The LLM is not a tool: it drives `agent_loop/` through this gateway.
//! The external seam is four items: `Gateway::new`, `Gateway::chat`,
//! `Gateway::chat_with_tools`, and `GatewayConfig::from_env`. Retry,
//! backoff, the wire format (OpenAI Chat Completions or Anthropic
//! Messages, `GATEWAY_API_FORMAT`), both thinking shapes, and status
//! mapping hide behind `chat` and `chat_with_tools`. Tool execution stays
//! in `research::agent_loop`.

mod anthropic;
mod client;
mod config;
pub(crate) mod reply;

pub use client::{Gateway, GatewayError};
pub use config::{ApiFormat, GatewayConfig, ModelParams, ResolvedParams};
pub use reply::{
    ChatMessage, LlmReply, ReplayBlocks, RequestedToolCall, TokenUsage, ToolChoice, ToolDef,
};
