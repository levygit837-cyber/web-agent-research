//! LLM gateway client: request, status mapping, retry, for both wire
//! formats (OpenAI Chat Completions, Anthropic Messages).
//!
//! The external seam is four items: `Gateway::new`, `Gateway::chat`,
//! `Gateway::chat_with_tools`, and `GatewayConfig::from_env`. Retry,
//! backoff, status mapping, and the format branch hide behind `chat` and
//! `chat_with_tools`; `with_client` and `backoff_delay` exist only as the
//! unit-test surface. The format is an enum branch, not a `trait`
//! (ADR-0007).

use std::time::Duration;

use super::anthropic;
use super::config::{ApiFormat, GatewayConfig};
use super::reply::LlmReply;
use super::request::{ChatMessage, ChatRequest, ToolChoice, ToolDef};
use super::response::{parse_reply, ChatResponse};

/// Failures of one gateway turn. Retryable variants are retried inside
/// `Gateway::chat` up to `GatewayConfig::max_attempts`; the caller only ever
/// sees the final error.
#[derive(Debug)]
pub enum GatewayError {
    /// Pre-flight: no key configured, no I/O attempted.
    MissingApiKey,
    /// 401: bad key, do NOT retry.
    Auth,
    /// 403: the key was accepted but access was refused (Anthropic
    /// `permission_error`, an upstream free-tier gate, ...). Carries the
    /// body's `error.message` when present. Not retryable.
    Forbidden(String),
    /// 429, including the upstream credit-wall. Retryable with backoff.
    RateLimited { retry_after_secs: Option<u64> },
    /// 5xx. Retryable.
    Server { status: u16 },
    /// Other 4xx (400, 404, 422, ...). Not retryable.
    Client { status: u16 },
    /// Per-attempt deadline hit. Retryable.
    Timeout,
    /// Connection/DNS/TLS failure. Retryable (transient network).
    Transport(String),
    /// 2xx body failed serde. Not retryable (deterministic).
    Parse(String),
    /// 2xx but `choices: []`. Not retryable.
    EmptyChoices,
    /// Model refusal text. Not retryable.
    Refused(String),
}

impl GatewayError {
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Server { .. } | Self::Timeout | Self::Transport(_)
        )
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingApiKey => write!(f, "GATEWAY_API_KEY is not set"),
            Self::Auth => write!(f, "gateway auth failed (bad API key)"),
            Self::Forbidden(detail) => write!(f, "gateway refused access (HTTP 403): {detail}"),
            Self::RateLimited { retry_after_secs } => match retry_after_secs {
                Some(secs) => write!(f, "gateway rate-limited, retry after {secs}s"),
                None => write!(f, "gateway rate-limited"),
            },
            Self::Server { status } => write!(f, "gateway server error (HTTP {status})"),
            Self::Client { status } => write!(f, "gateway client error (HTTP {status})"),
            Self::Timeout => write!(f, "gateway request timed out"),
            Self::Transport(detail) => write!(f, "gateway transport error: {detail}"),
            Self::Parse(detail) => write!(f, "gateway response parse error: {detail}"),
            Self::EmptyChoices => write!(f, "gateway returned no choices"),
            Self::Refused(text) => write!(f, "model refused the turn: {text}"),
        }
    }
}

impl std::error::Error for GatewayError {}

pub struct Gateway {
    config: GatewayConfig,
    client: reqwest::Client,
}

impl Gateway {
    pub fn new(config: GatewayConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .expect("gateway reqwest client builds with a timeout");
        Self::with_client(config, client)
    }

    /// Test seam: caller-supplied HTTP client (e.g. short timeouts).
    pub(crate) fn with_client(config: GatewayConfig, client: reqwest::Client) -> Self {
        Self { config, client }
    }

    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmReply, GatewayError> {
        self.chat_with_body(messages, None, None).await
    }

    /// Same path as [`Gateway::chat`], but offers native function-calling
    /// tools to the model and returns the requested calls on [`LlmReply`].
    /// Execution of the calls stays in `research::agent_loop`.
    pub async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDef],
        tool_choice: Option<ToolChoice>,
    ) -> Result<LlmReply, GatewayError> {
        self.chat_with_body(messages, Some(tools.to_vec()), tool_choice)
            .await
    }

    async fn chat_with_body(
        &self,
        messages: &[ChatMessage],
        tools: Option<Vec<ToolDef>>,
        tool_choice: Option<ToolChoice>,
    ) -> Result<LlmReply, GatewayError> {
        if self.config.api_key().is_empty() {
            return Err(GatewayError::MissingApiKey);
        }
        let params = self.config.effective_params(&self.config.model);
        let extra = self.config.extra_body.as_ref();
        // Built once: every retry sends the identical body.
        let (url, body) = match self.config.api_format {
            ApiFormat::Openai => (
                format!("{}/chat/completions", self.config.base_url),
                ChatRequest::from_resolved_with_tools(
                    &params,
                    messages.to_vec(),
                    tools,
                    tool_choice,
                )
                .to_wire_value(extra),
            ),
            ApiFormat::Anthropic => (
                format!("{}/messages", self.config.base_url),
                anthropic::request_body(
                    &params,
                    messages,
                    tools.as_deref(),
                    tool_choice.as_ref(),
                    extra,
                ),
            ),
        };
        let attempts = self.config.max_attempts.max(1);

        let mut last_err: Option<GatewayError> = None;
        for attempt in 1..=attempts {
            match self.try_once(&url, &body).await {
                Ok(reply) => return Ok(reply),
                Err(err) => {
                    let retry_after = match &err {
                        GatewayError::RateLimited { retry_after_secs } => *retry_after_secs,
                        _ => None,
                    };
                    if !err.is_retryable() || attempt == attempts {
                        return Err(err);
                    }
                    last_err = Some(err);
                    tokio::time::sleep(backoff_delay(attempt, retry_after)).await;
                }
            }
        }
        // `attempts >= 1`, so the loop always runs; this only satisfies the type checker.
        Err(last_err.unwrap_or(GatewayError::Transport("no attempts ran".to_owned())))
    }

    async fn try_once(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<LlmReply, GatewayError> {
        let request = self.client.post(url).json(body);
        let request = match self.config.api_format {
            ApiFormat::Openai => request.bearer_auth(self.config.api_key()),
            ApiFormat::Anthropic => request
                .header("x-api-key", self.config.api_key())
                .header("anthropic-version", anthropic::ANTHROPIC_VERSION),
        };
        let resp = request.send().await.map_err(|err| {
            if err.is_timeout() {
                GatewayError::Timeout
            } else {
                GatewayError::Transport(err.to_string())
            }
        })?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(GatewayError::Auth);
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            let body = resp.text().await.unwrap_or_default();
            return Err(GatewayError::Forbidden(error_message(&body)));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(GatewayError::RateLimited {
                retry_after_secs: parse_retry_after(resp.headers()),
            });
        }
        if status.is_server_error() {
            return Err(GatewayError::Server {
                status: status.as_u16(),
            });
        }
        if status.is_client_error() {
            return Err(GatewayError::Client {
                status: status.as_u16(),
            });
        }

        match self.config.api_format {
            ApiFormat::Openai => {
                let chat: ChatResponse = resp
                    .json()
                    .await
                    .map_err(|err| GatewayError::Parse(err.to_string()))?;
                parse_reply(chat)
            }
            ApiFormat::Anthropic => {
                let message: anthropic::MessagesResponse = resp
                    .json()
                    .await
                    .map_err(|err| GatewayError::Parse(err.to_string()))?;
                anthropic::parse_response(message)
            }
        }
    }
}

/// `error.message` from an error body (both formats nest it there), else
/// the first 200 chars of the raw body, else a placeholder.
fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            let trimmed: String = body.trim().chars().take(200).collect();
            if trimmed.is_empty() {
                "no error detail".to_owned()
            } else {
                trimmed
            }
        })
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Exponential backoff: 1 s x 2^(attempt-1), capped at 8 s. An explicit
/// `Retry-After` wins for that sleep, capped at 60 s so one header cannot
/// stall a turn past one timeout window.
pub(crate) fn backoff_delay(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    if let Some(secs) = retry_after_secs {
        return Duration::from_secs(secs.min(60));
    }
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(3))
}

#[cfg(test)]
mod tests {
    mod anthropic_wire;
    mod helpers;
    mod openai_wire;
    mod retry_and_status;
    mod tool_calls;
}
