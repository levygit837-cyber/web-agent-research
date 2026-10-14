//! LLM gateway: the model behind `research::agent_loop`. Never imports `web`.
//!
//! The LLM is not a tool: it drives `agent_loop/` through this gateway.
//! The external seam is `Gateway::new`, `Gateway::chat`,
//! `Gateway::chat_with_tools`, `Gateway::chat_observed` (the same call plus
//! a closure that receives each `LlmEvent`), and `GatewayConfig::from_env`.
//! Retry, backoff, the wire format (OpenAI Chat Completions or Anthropic
//! Messages, `GATEWAY_API_FORMAT`), SSE token streaming (OpenAI format,
//! `GATEWAY_STREAM`), both thinking shapes, and status mapping hide behind
//! them. Tool execution stays in `research::agent_loop`.

mod anthropic;
mod client;
mod config;
pub(crate) mod reply;
pub(crate) mod request;
pub(crate) mod response;
mod stream;

pub use client::{Gateway, GatewayError};
pub use config::{ApiFormat, GatewayConfig, ModelParams, ResolvedParams};
pub use reply::{LlmReply, RequestedToolCall, TokenUsage};
pub use request::{ChatMessage, ReplayBlocks, ToolChoice, ToolDef};
pub use stream::LlmEvent;
