//! LLM gateway client: request, status mapping, retry, for both wire
//! formats (OpenAI Chat Completions, Anthropic Messages).
//!
//! The external seam is `Gateway::new`, `Gateway::with_deadline`,
//! `Gateway::chat`, `Gateway::chat_with_tools`, and `GatewayConfig::from_env`.
//! Retry, backoff, status mapping, and the format branch hide behind `chat`
//! and `chat_with_tools`; `with_client` and `backoff_delay` exist only as the
//! unit-test surface. The format is an enum branch, not a `trait`
//! (ADR-0007).

use std::time::{Duration, SystemTime};

use rand::Rng;
use tokio::time::Instant;

use super::anthropic;
use super::config::{ApiFormat, GatewayConfig};
use super::reply::LlmReply;
use super::request::{ChatMessage, ChatRequest, ToolChoice, ToolDef};
use super::response::{parse_reply, ChatResponse};

/// Upper bound on any upstream error text carried in a [`GatewayError`],
/// so a gateway that echoes a whole request cannot flood stderr.
const MAX_DETAIL_CHARS: usize = 300;

/// TCP/TLS connect bound per attempt. A gateway that cannot even accept a
/// connection in this time is down; the rest of `request_timeout` is left
/// for generation.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Failures of one gateway turn. Retryable variants are retried inside
/// `Gateway::chat` up to `GatewayConfig::max_attempts`; the caller only ever
/// sees the final error. [`GatewayError::is_retryable`] splits them into
/// "unavailable, try later" and "rejected, fix the request".
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
    /// 5xx with the bounded upstream message. Retryable except 501 (Not
    /// Implemented) and 505 (HTTP Version Not Supported), which a retry
    /// cannot fix. `retry_after_secs` is read on 503 and 529.
    Server {
        status: u16,
        detail: String,
        retry_after_secs: Option<u64>,
    },
    /// Other 4xx (400, 404, 422, ...) with the bounded upstream message.
    /// Not retryable.
    Client { status: u16, detail: String },
    /// An error object inside a 2xx body (OpenAI `{"error": ...}`,
    /// Anthropic `{"type": "error", ...}`). Retryable when its kind says
    /// the upstream is overloaded, rate-limited, or failed internally.
    ErrorBody {
        kind: String,
        message: String,
        retryable: bool,
    },
    /// Per-attempt deadline hit while sending or reading the body.
    /// Retried at most once per call.
    Timeout,
    /// Connection/DNS/TLS failure or a body cut mid-read. Retryable
    /// (transient network). Never carries the URL.
    Transport(String),
    /// The request could not be built (reqwest builder error). Not
    /// retryable: the same request fails the same way.
    InvalidRequest(String),
    /// 2xx body failed serde. Not retryable (deterministic).
    Parse(String),
    /// 2xx but `choices: []`. Not retryable.
    EmptyChoices,
    /// Model refusal text. Not retryable.
    Refused(String),
}

impl GatewayError {
    /// Whether the same request may succeed later: rate limits, 5xx other
    /// than 501/505, upstream-overload error bodies, timeouts, and
    /// transport failures. Everything else is a rejection.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Timeout | Self::Transport(_) => true,
            Self::Server { status, .. } => !matches!(status, 501 | 505),
            Self::ErrorBody { retryable, .. } => *retryable,
            _ => false,
        }
    }

    fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_secs }
            | Self::Server {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
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
            Self::Server { status, detail, .. } => {
                write!(f, "gateway server error (HTTP {status}): {detail}")
            }
            Self::Client { status, detail } => {
                write!(f, "gateway client error (HTTP {status}): {detail}")
            }
            Self::ErrorBody { kind, message, .. } => {
                write!(f, "gateway returned an error body ({kind}): {message}")
            }
            Self::Timeout => write!(f, "gateway request timed out"),
            Self::Transport(detail) => write!(f, "gateway transport error: {detail}"),
            Self::InvalidRequest(detail) => write!(f, "gateway request is invalid: {detail}"),
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
    /// End of the whole run, when the caller set one: no attempt's timeout
    /// and no backoff sleep reaches past it.
    deadline: Option<Instant>,
}

impl Gateway {
    pub fn new(config: GatewayConfig) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(config.request_timeout)
            .build()
            .expect("gateway reqwest client builds with a timeout");
        Self::with_client(config, client)
    }

    /// Test seam: caller-supplied HTTP client (e.g. short timeouts).
    pub(crate) fn with_client(config: GatewayConfig, client: reqwest::Client) -> Self {
        Self {
            config,
            client,
            deadline: None,
        }
    }

    /// Bound every later call by `deadline`: each attempt's timeout is
    /// clamped to the time left, and a retry whose backoff would end past
    /// it is not attempted (the last error is returned instead).
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
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
        let mut timeouts: u32 = 0;

        let mut attempt: u32 = 1;
        loop {
            let attempt_timeout = match self.time_left() {
                Some(left) if left.is_zero() => return Err(GatewayError::Timeout),
                Some(left) if left < self.config.request_timeout => Some(left),
                _ => None,
            };
            let err = match self.try_once(&url, &body, attempt_timeout).await {
                Ok(reply) => return Ok(reply),
                Err(err) => err,
            };
            if matches!(err, GatewayError::Timeout) {
                timeouts += 1;
            }
            // A gateway that hung once usually hangs again: one retry, then
            // the run moves on instead of spending every attempt waiting.
            let timeout_spent = matches!(err, GatewayError::Timeout) && timeouts > 1;
            if !err.is_retryable() || timeout_spent || attempt >= attempts {
                return Err(err);
            }
            let delay = backoff_delay(attempt, err.retry_after_secs());
            if self.time_left().is_some_and(|left| left <= delay) {
                return Err(err);
            }
            tracing::info!("gateway attempt {attempt} failed ({err}); retrying in {delay:?}");
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    fn time_left(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    async fn try_once(
        &self,
        url: &str,
        body: &serde_json::Value,
        attempt_timeout: Option<Duration>,
    ) -> Result<LlmReply, GatewayError> {
        let mut request = self.client.post(url).json(body);
        if let Some(timeout) = attempt_timeout {
            request = request.timeout(timeout);
        }
        let request = match self.config.api_format {
            ApiFormat::Openai => request.bearer_auth(self.config.api_key()),
            ApiFormat::Anthropic => request
                .header("x-api-key", self.config.api_key())
                .header("anthropic-version", anthropic::ANTHROPIC_VERSION),
        };
        let resp = request.send().await.map_err(map_reqwest_error)?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(GatewayError::Auth);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(GatewayError::RateLimited {
                retry_after_secs: parse_retry_after(resp.headers(), SystemTime::now()),
            });
        }
        if status.is_client_error() || status.is_server_error() {
            let retry_after_secs = matches!(status.as_u16(), 503 | 529)
                .then(|| parse_retry_after(resp.headers(), SystemTime::now()))
                .flatten();
            // The detail is best-effort: a body that fails to arrive must
            // not hide the status that already did.
            let raw = resp.bytes().await.unwrap_or_default();
            let detail = error_message(&String::from_utf8_lossy(&raw));
            return Err(match status.as_u16() {
                403 => GatewayError::Forbidden(detail),
                code if status.is_server_error() => GatewayError::Server {
                    status: code,
                    detail,
                    retry_after_secs,
                },
                code => GatewayError::Client {
                    status: code,
                    detail,
                },
            });
        }

        // Bytes first: a timeout or reset while the body streams in is a
        // transport failure (retryable), not a parse failure.
        let raw = resp.bytes().await.map_err(map_reqwest_error)?;
        match self.config.api_format {
            ApiFormat::Openai => {
                let chat: ChatResponse = serde_json::from_slice(&raw)
                    .map_err(|err| GatewayError::Parse(err.to_string()))?;
                parse_reply(chat)
            }
            ApiFormat::Anthropic => {
                let message: anthropic::MessagesResponse = serde_json::from_slice(&raw)
                    .map_err(|err| GatewayError::Parse(err.to_string()))?;
                anthropic::parse_response(message)
            }
        }
    }
}

/// Send/read failures: timeout, builder (not retryable), or transport. The
/// URL is stripped so credentials in `GATEWAY_BASE_URL` never reach stderr;
/// the source chain is kept, since reqwest's own message is generic.
fn map_reqwest_error(err: reqwest::Error) -> GatewayError {
    if err.is_timeout() {
        return GatewayError::Timeout;
    }
    let is_builder = err.is_builder();
    let err = err.without_url();
    let mut detail = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    if is_builder {
        GatewayError::InvalidRequest(detail)
    } else {
        GatewayError::Transport(detail)
    }
}

fn bounded(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_DETAIL_CHARS {
        return trimmed.to_owned();
    }
    let mut cut: String = trimmed.chars().take(MAX_DETAIL_CHARS).collect();
    cut.push('…');
    cut
}

/// `error.message` from an error body (both formats nest it there), else
/// the raw body, else a placeholder; bounded to [`MAX_DETAIL_CHARS`].
fn error_message(body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned());
    let message = bounded(&message);
    if message.is_empty() {
        "no error detail".to_owned()
    } else {
        message
    }
}

/// Error kinds (`type` or `code`) that mean "the upstream is busy or broke",
/// so the same request may succeed later.
const RETRYABLE_ERROR_KINDS: &[&str] = &[
    "overloaded_error",
    "api_error",
    "rate_limit_error",
    "rate_limit_exceeded",
    "server_error",
];

/// The error object of a 2xx body, in either format: `type` (Anthropic,
/// OpenAI) or `code` names the kind; a bare string is the message.
pub(crate) fn error_object(error: &serde_json::Value) -> GatewayError {
    let field = |name: &str| error.get(name).and_then(serde_json::Value::as_str);
    let kinds = [field("type"), field("code")];
    let retryable = kinds
        .iter()
        .flatten()
        .any(|kind| RETRYABLE_ERROR_KINDS.contains(kind));
    let kind = kinds.into_iter().flatten().next().unwrap_or("unknown");
    let message = field("message")
        .or_else(|| error.as_str())
        .map(bounded)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "no error detail".to_owned());
    GatewayError::ErrorBody {
        kind: kind.to_owned(),
        message,
        retryable,
    }
}

/// `Retry-After` as delta-seconds or an HTTP-date (RFC 9110 §10.2.3); a
/// date in the past means "now" (0 s).
fn parse_retry_after(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<u64> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(secs);
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).map_or(0, |left| {
        left.as_secs() + u64::from(left.subsec_nanos() > 0)
    }))
}

/// Exponential ceiling: 1 s x 2^(attempt-1), capped at 8 s.
pub(crate) fn backoff_ceiling(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(3))
}

/// Sleep before the next attempt. An explicit `Retry-After` wins, capped at
/// 60 s so one header cannot stall a turn past one timeout window;
/// otherwise full jitter: uniform in `0..=backoff_ceiling(attempt)`, so
/// parallel runs that failed together do not retry together.
pub(crate) fn backoff_delay(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    if let Some(secs) = retry_after_secs {
        return Duration::from_secs(secs.min(60));
    }
    let ceiling_ms = backoff_ceiling(attempt).as_millis() as u64;
    Duration::from_millis(rand::rng().random_range(0..=ceiling_ms))
}

#[cfg(test)]
mod tests {
    mod anthropic_wire;
    mod error_bodies;
    mod helpers;
    mod openai_wire;
    mod retry_and_status;
    mod tool_calls;
}
