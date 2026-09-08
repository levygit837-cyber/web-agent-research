//! OpenAI-compatible gateway engine: the LLM behind `agent_loop/`.
//!
//! The LLM is not a tool: it drives `agent_loop/` through this gateway.
//! The external seam is four items: `Gateway::new`, `Gateway::chat`,
//! `Gateway::chat_with_tools`, and `GatewayConfig::from_env`. Retry,
//! backoff, both thinking shapes, and status mapping hide behind `chat`
//! and `chat_with_tools`. Tool execution stays in `agent_loop/` (issue #12).

mod client;
mod config;
pub(crate) mod reply;

pub use client::{Gateway, GatewayError};
pub use config::{GatewayConfig, ModelParams, ResolvedParams};
pub use reply::{ChatMessage, LlmReply, RequestedToolCall, TokenUsage, ToolChoice, ToolDef};
