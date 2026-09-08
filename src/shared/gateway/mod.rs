//! OpenAI-compatible gateway engine: the LLM behind `agent_loop/`.
//!
//! The LLM is not a tool: it drives `agent_loop/` through this gateway.
//! The external seam is three items: `Gateway::new`, `Gateway::chat`, and
//! `GatewayConfig::from_env`. Retry, backoff, both thinking shapes, and
//! status mapping hide behind `chat`.

mod client;
mod config;
pub(crate) mod reply;

pub use client::{Gateway, GatewayError};
pub use config::{GatewayConfig, ModelParams, ResolvedParams};
pub use reply::{ChatMessage, LlmReply, TokenUsage};
