//! OpenAI-compatible gateway client: request, status mapping, retry.
//!
//! The external seam is four items: `Gateway::new`, `Gateway::chat`,
//! `Gateway::chat_with_tools`, and `GatewayConfig::from_env`. Retry,
//! backoff, and status mapping hide behind `chat` and `chat_with_tools`;
//! `with_client` and `backoff_delay` exist only as the unit-test surface
//! (no second adapter means no `trait`, per ADR-0004).

use std::time::Duration;

use super::config::GatewayConfig;
use super::reply::{
    parse_reply, ChatMessage, ChatRequest, ChatResponse, LlmReply, ToolChoice, ToolDef,
};

/// Failures of one gateway turn. Retryable variants are retried inside
/// `Gateway::chat` up to `GatewayConfig::max_attempts`; the caller only ever
/// sees the final error.
#[derive(Debug)]
pub enum GatewayError {
    /// Pre-flight: no key configured, no I/O attempted.
    MissingApiKey,
    /// 401/403: bad key, do NOT retry.
    Auth,
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
    /// Execution of the calls stays in the agent loop (issue #12).
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
        let url = format!("{}/chat/completions", self.config.base_url);
        let attempts = self.config.max_attempts.max(1);

        let mut last_err: Option<GatewayError> = None;
        for attempt in 1..=attempts {
            let body = match (&tools, &tool_choice) {
                (None, None) => ChatRequest::from_resolved(&params, messages.to_vec()),
                _ => ChatRequest::from_resolved_with_tools(
                    &params,
                    messages.to_vec(),
                    tools.clone(),
                    tool_choice.clone(),
                ),
            };
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

    async fn try_once(&self, url: &str, body: &ChatRequest) -> Result<LlmReply, GatewayError> {
        let resp = self
            .client
            .post(url)
            .bearer_auth(self.config.api_key())
            .json(body)
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    GatewayError::Timeout
                } else {
                    GatewayError::Transport(err.to_string())
                }
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(GatewayError::Auth);
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

        let chat: ChatResponse = resp
            .json()
            .await
            .map_err(|err| GatewayError::Parse(err.to_string()))?;
        parse_reply(chat)
    }
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
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    const SUCCESS_BODY: &str = r#"{
        "choices": [{
            "message": {"role": "assistant", "content": "gateway ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }"#;

    struct CannedResponse {
        status: u16,
        retry_after: Option<&'static str>,
        body: &'static str,
        delay_before_response: Duration,
    }

    impl CannedResponse {
        fn ok() -> Self {
            Self {
                status: 200,
                retry_after: None,
                body: SUCCESS_BODY,
                delay_before_response: Duration::ZERO,
            }
        }

        fn status(status: u16) -> Self {
            Self {
                status,
                retry_after: None,
                body: "{}",
                delay_before_response: Duration::ZERO,
            }
        }
    }

    /// Serve canned responses from a background thread over plain HTTP.
    /// Returns the base URL (no trailing slash) and the server-side hit count.
    /// Uses `std::net::TcpListener` so no new deps are needed.
    fn spawn_server(responses: Vec<CannedResponse>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        let addr = listener.local_addr().expect("listener has an address");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_thread = Arc::clone(&hits);
        let total = responses.len();
        std::thread::spawn(move || {
            for canned in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                read_request(&mut stream);
                if !canned.delay_before_response.is_zero() {
                    std::thread::sleep(canned.delay_before_response);
                }
                let mut head = format!(
                    "HTTP/1.1 {} x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    canned.status,
                    canned.body.len()
                );
                if let Some(retry_after) = canned.retry_after {
                    head.push_str(&format!("Retry-After: {retry_after}\r\n"));
                }
                let _ = stream.write_all(format!("{head}\r\n{}", canned.body).as_bytes());
                hits_in_thread.fetch_add(1, Ordering::SeqCst);
                if hits_in_thread.load(Ordering::SeqCst) >= total {
                    return;
                }
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn read_request(stream: &mut std::net::TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        // Headers first.
        loop {
            let Ok(n) = stream.read(&mut chunk) else {
                return;
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(buf.len(), |i| i + 4);
        let content_length = String::from_utf8_lossy(&buf[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.trim().eq_ignore_ascii_case("content-length"))
                    .then(|| value.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        let mut remaining = content_length.saturating_sub(buf.len() - header_end);
        while remaining > 0 {
            let Ok(n) = stream.read(&mut chunk) else {
                return;
            };
            if n == 0 {
                return;
            }
            remaining = remaining.saturating_sub(n);
        }
    }

    fn messages() -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: "user".to_owned(),
            content: "Reply with exactly: gateway ok".to_owned(),
        }]
    }

    fn single_attempt_gateway(base_url: String) -> Gateway {
        Gateway::with_client(
            GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
                .with_max_attempts(1),
            reqwest::Client::new(),
        )
    }

    type StatusCheck = fn(&GatewayError) -> bool;

    #[tokio::test]
    async fn status_mapping() {
        let cases: Vec<(CannedResponse, StatusCheck)> = vec![
            (CannedResponse::status(401), |e| {
                matches!(e, GatewayError::Auth) && !e.is_retryable()
            }),
            (
                CannedResponse {
                    status: 429,
                    retry_after: Some("7"),
                    body: "{}",
                    delay_before_response: Duration::ZERO,
                },
                |e| {
                    matches!(
                        e,
                        GatewayError::RateLimited {
                            retry_after_secs: Some(7)
                        }
                    ) && e.is_retryable()
                },
            ),
            (CannedResponse::status(500), |e| {
                matches!(e, GatewayError::Server { status: 500 }) && e.is_retryable()
            }),
            (CannedResponse::status(404), |e| {
                matches!(e, GatewayError::Client { status: 404 }) && !e.is_retryable()
            }),
        ];
        for (canned, check) in cases {
            let status = canned.status;
            let (base_url, _) = spawn_server(vec![canned]);
            let err = single_attempt_gateway(base_url)
                .chat(&messages())
                .await
                .expect_err(&format!("HTTP {status} must be an error"));
            assert!(check(&err), "HTTP {status} mapped wrong: {err:?} ({err})");
        }
    }

    #[tokio::test]
    async fn timeout_maps_and_retries() {
        let (base_url, _) = spawn_server(vec![CannedResponse {
            status: 200,
            retry_after: None,
            body: SUCCESS_BODY,
            delay_before_response: Duration::from_secs(5),
        }]);
        let gateway = Gateway::with_client(
            GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
                .with_max_attempts(1),
            reqwest::Client::builder()
                .timeout(Duration::from_millis(100))
                .build()
                .expect("test client builds"),
        );
        let err = gateway
            .chat(&messages())
            .await
            .expect_err("slow server must time out");
        assert!(
            matches!(err, GatewayError::Timeout) && err.is_retryable(),
            "slow server must map to retryable Timeout, got {err:?} ({err})"
        );
    }

    #[tokio::test]
    async fn retry_then_success() {
        let (base_url, hits) =
            spawn_server(vec![CannedResponse::status(429), CannedResponse::ok()]);
        let gateway = Gateway::with_client(
            GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
                .with_max_attempts(3),
            reqwest::Client::new(),
        );
        let reply = gateway
            .chat(&messages())
            .await
            .expect("second attempt must succeed");
        assert_eq!(reply.output, "gateway ok");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn no_retry_on_auth() {
        let (base_url, hits) = spawn_server(vec![CannedResponse::status(401)]);
        let gateway = Gateway::with_client(
            GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
                .with_max_attempts(3),
            reqwest::Client::new(),
        );
        let err = gateway.chat(&messages()).await.expect_err("401 must fail");
        assert!(matches!(err, GatewayError::Auth), "got {err:?} ({err})");
        // Give a would-be retry no chance to land: exactly one server hit.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
    const TOOL_CALL_BODY: &str = r#"{
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
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
    }"#;

    fn get_time_tools() -> Vec<ToolDef> {
        vec![ToolDef {
            name: "get_time".to_owned(),
            description: "Returns the current time.".to_owned(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]
    }

    #[tokio::test]
    async fn chat_with_tools_returns_tool_calls() {
        let (base_url, hits) = spawn_server(vec![CannedResponse {
            status: 200,
            retry_after: None,
            body: TOOL_CALL_BODY,
            delay_before_response: Duration::ZERO,
        }]);
        let gateway = single_attempt_gateway(base_url);
        let reply = gateway
            .chat_with_tools(&messages(), &get_time_tools(), Some(ToolChoice::Auto))
            .await
            .expect("tool_calls body must parse");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "chatcmpl-tool-abc123");
        assert_eq!(reply.tool_calls[0].name, "get_time");
        assert_eq!(
            reply.tool_calls[0]
                .parsed_arguments()
                .expect("arguments must parse"),
            serde_json::json!({})
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn chat_with_tools_auth_does_not_retry() {
        let (base_url, hits) = spawn_server(vec![CannedResponse::status(401)]);
        let gateway = Gateway::with_client(
            GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
                .with_max_attempts(3),
            reqwest::Client::new(),
        );
        let err = gateway
            .chat_with_tools(&messages(), &get_time_tools(), Some(ToolChoice::Auto))
            .await
            .expect_err("401 must fail");
        assert!(matches!(err, GatewayError::Auth), "got {err:?} ({err})");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backoff_shape() {
        assert_eq!(backoff_delay(1, None), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, None), Duration::from_secs(2));
        assert_eq!(backoff_delay(2, Some(30)), Duration::from_secs(30));
        assert_eq!(backoff_delay(9, None), Duration::from_secs(8));
        assert_eq!(backoff_delay(1, Some(3600)), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn missing_key_never_touches_io() {
        let gateway = Gateway::new(GatewayConfig::new(
            "http://127.0.0.1:1".to_owned(),
            String::new(),
            "m".to_owned(),
        ));
        let err = gateway
            .chat(&messages())
            .await
            .expect_err("empty key must fail pre-flight");
        assert!(matches!(err, GatewayError::MissingApiKey));
    }
}
