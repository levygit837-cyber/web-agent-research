//! `search` Mode: classify input, run the agent loop, synthesize.
//!
//! Deep module for one Mode end to end (ADR-0004): `run_research()` owns
//! gateway construction, `run_loop` wiring, JSONL append, and the response
//! shape. The clap CLI and the future Axum handler stay thin adapters over
//! this interface.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::shared::agent_loop::{run_loop, LoopBudget, LoopError, LoopInput, ToolRegistry};
use crate::shared::gateway::{Gateway, GatewayConfig, TokenUsage};
use crate::shared::session::{
    append_header, append_turn, Evidence, SessionHeader, SessionUsage, TurnRow,
};
use crate::shared::types::synthesis::{Citation, Synthesis, SynthesisSize, ThemeSection};

/// Default turn budget when the caller omits `max_turns`.
pub fn default_max_turns() -> u32 {
    LoopBudget::default().max_turns
}

fn default_size() -> SynthesisSize {
    SynthesisSize::default()
}

mod size_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::shared::types::synthesis::SynthesisSize;

    pub fn serialize<S>(size: &SynthesisSize, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(size.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SynthesisSize, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "small" => Ok(SynthesisSize::Small),
            "medium" => Ok(SynthesisSize::Medium),
            "large" => Ok(SynthesisSize::Large),
            "deep" => Ok(SynthesisSize::Deep),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["small", "medium", "large", "deep"],
            )),
        }
    }
}

/// Research input; deserializes from the future POST body as-is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResearchRequest {
    /// Research goal, user words. Non-empty after trim.
    pub goal: String,
    /// Requested synthesis size; echoed into the response unchanged.
    #[serde(with = "size_serde", default = "default_size")]
    pub size: SynthesisSize,
    /// Total gateway calls allowed. Must be >= 1.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Defaults to generated `<unix-secs>-<pid>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Defaults to `sessions/<id>.jsonl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_out: Option<PathBuf>,
}

/// One inline `[title](url)` citation, in document order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CitationDTO {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One `## ` section of the answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ThemeDTO {
    pub title: String,
    pub points: Vec<String>,
    pub citations: Vec<CitationDTO>,
}

/// Typed final answer; mirrors [`Synthesis`] 1:1 on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SynthesisDTO {
    #[serde(with = "size_serde")]
    pub size: SynthesisSize,
    pub summary: String,
    pub themes: Vec<ThemeDTO>,
    pub citations: Vec<CitationDTO>,
}

/// Per-run token counts; saturating sums.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UsageDTO {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Research output; serializes to the future 200 body and `--json` stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResearchResponse {
    pub session_id: String,
    pub synthesis: SynthesisDTO,
    pub turns_used: u32,
    pub evidence_urls: Vec<String>,
    pub usage: UsageDTO,
}

/// Failures of one research run; each maps to a CLI exit code and a future
/// HTTP status (`NotConfigured→400`, `GatewayExhausted→502`,
/// `ToolFailure→502`, `BudgetExhausted→504`, `Io→500`).
#[derive(Debug)]
pub enum ResearchError {
    /// Empty goal, `max_turns == 0`, missing `GATEWAY_API_KEY` — no I/O attempted.
    NotConfigured(String),
    /// Gateway retries exhausted, incl. the 429-credit wall.
    GatewayExhausted(String),
    /// Invalid tool call or tool failure after `max_repairs`.
    ToolFailure(String),
    /// `max_turns` consumed with no FINAL parsed.
    BudgetExhausted { turns: u32 },
    /// Session file write failure.
    Io(String),
}

impl std::fmt::Display for ResearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured(reason) => write!(f, "research not configured: {reason}"),
            Self::GatewayExhausted(reason) => write!(f, "gateway exhausted: {reason}"),
            Self::ToolFailure(reason) => write!(f, "tool failure: {reason}"),
            Self::BudgetExhausted { turns } => {
                write!(f, "turn budget exhausted after {turns} turns")
            }
            Self::Io(reason) => write!(f, "session write failed: {reason}"),
        }
    }
}

impl std::error::Error for ResearchError {}

impl From<Citation> for CitationDTO {
    fn from(citation: Citation) -> Self {
        Self {
            url: citation.url,
            title: citation.title,
        }
    }
}

impl From<ThemeSection> for ThemeDTO {
    fn from(section: ThemeSection) -> Self {
        Self {
            title: section.title,
            points: section.points,
            citations: section
                .citations
                .into_iter()
                .map(CitationDTO::from)
                .collect(),
        }
    }
}

impl From<Synthesis> for SynthesisDTO {
    fn from(synthesis: Synthesis) -> Self {
        Self {
            size: synthesis.size,
            summary: synthesis.summary,
            themes: synthesis.themes.into_iter().map(ThemeDTO::from).collect(),
            citations: synthesis
                .citations
                .into_iter()
                .map(CitationDTO::from)
                .collect(),
        }
    }
}

impl From<TokenUsage> for UsageDTO {
    fn from(usage: TokenUsage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

impl From<UsageDTO> for SessionUsage {
    fn from(usage: UsageDTO) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

fn map_loop_error(err: LoopError) -> ResearchError {
    match err {
        LoopError::NoStart(reason) => ResearchError::NotConfigured(reason),
        LoopError::Gateway(gateway_err) => {
            let message = gateway_err.to_string();
            if matches!(
                gateway_err,
                crate::shared::gateway::GatewayError::RateLimited { .. }
            ) {
                ResearchError::GatewayExhausted(format!("{message}; retry later or abort"))
            } else {
                ResearchError::GatewayExhausted(message)
            }
        }
        LoopError::InvalidToolCall { reason } | LoopError::ToolFailed { reason } => {
            ResearchError::ToolFailure(reason)
        }
        LoopError::BudgetExhausted { turns } => ResearchError::BudgetExhausted { turns },
    }
}

fn generate_session_id() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}-{}", std::process::id())
}

/// Run one research session end to end: gateway from env, live tool
/// registry, agent loop, then JSONL append.
///
/// Owns `req`; borrows nothing. Exactly one gateway call per turn.
/// `allowed_tools` is fixed to `["search", "fetch"]` in registry order.
/// The key comes only from env (`GATEWAY_API_KEY`), never from the request.
pub async fn run_research(req: ResearchRequest) -> Result<ResearchResponse, ResearchError> {
    if req.goal.trim().is_empty() {
        return Err(ResearchError::NotConfigured(
            "research goal is empty".to_owned(),
        ));
    }
    if req.max_turns < 1 {
        return Err(ResearchError::NotConfigured(
            "max_turns must be >= 1".to_owned(),
        ));
    }
    let config =
        GatewayConfig::from_env().map_err(|err| ResearchError::NotConfigured(err.to_string()))?;
    let gateway = Gateway::new(config);
    let tools = ToolRegistry::live();
    let input = LoopInput {
        goal: req.goal.clone(),
        size: req.size,
        allowed_tools: vec!["search".to_owned(), "fetch".to_owned()],
    };
    let budget = LoopBudget {
        max_turns: req.max_turns,
        ..LoopBudget::default()
    };
    let report = run_loop(&gateway, &tools, &input, &budget)
        .await
        .map_err(map_loop_error)?;

    let session_id = req.session_id.clone().unwrap_or_else(generate_session_id);
    let session_path = req
        .session_out
        .clone()
        .unwrap_or_else(|| PathBuf::from("sessions").join(format!("{session_id}.jsonl")));
    let synthesis_dto = SynthesisDTO::from(report.synthesis);
    let evidence_urls: Vec<String> = report
        .evidence
        .iter()
        .filter_map(|item| item.url.clone())
        .collect();
    let usage_dto = UsageDTO::from(report.usage);

    let header = SessionHeader::new(
        session_id.clone(),
        req.goal.clone(),
        req.size.as_str().to_owned(),
    );
    append_header(&session_path, &header).map_err(|err| ResearchError::Io(err.to_string()))?;
    let synthesis_value =
        serde_json::to_value(&synthesis_dto).map_err(|err| ResearchError::Io(err.to_string()))?;
    for turn in 1..=report.turns_used {
        let evidence: Vec<Evidence> = report
            .evidence
            .iter()
            .filter(|item| item.turn == turn)
            .filter_map(|item| {
                item.url
                    .clone()
                    .map(|url| Evidence::new(url, item.excerpt.clone()))
            })
            .collect();
        let is_final = turn == report.turns_used;
        let row = TurnRow::new(
            session_id.clone(),
            turn,
            evidence,
            is_final.then(|| synthesis_value.clone()),
            if is_final {
                SessionUsage::from(usage_dto.clone())
            } else {
                SessionUsage::default()
            },
        );
        append_turn(&session_path, &row).map_err(|err| ResearchError::Io(err.to_string()))?;
    }

    Ok(ResearchResponse {
        session_id,
        synthesis: synthesis_dto,
        turns_used: report.turns_used,
        evidence_urls,
        usage: usage_dto,
    })
}

/// Pure printer for human stdout: summary, `##` theme sections with `-`
/// points, then a numbered `Sources:` `[title](url)` list in document order.
pub fn render(response: &ResearchResponse) -> String {
    let mut out = String::new();
    out.push_str(&response.synthesis.summary);
    out.push_str("\n\n");
    for theme in &response.synthesis.themes {
        out.push_str("## ");
        out.push_str(&theme.title);
        out.push_str("\n\n");
        for point in &theme.points {
            out.push_str("- ");
            out.push_str(point);
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str("Sources:\n");
    for (index, citation) in response.synthesis.citations.iter().enumerate() {
        let title = citation.title.as_deref().unwrap_or(&citation.url);
        out.push_str(&format!("{}. [{}]({})\n", index + 1, title, citation.url));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvGuard {
        keys: Vec<&'static str>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn lock(keys: Vec<&'static str>) -> Self {
            let lock = ENV_LOCK.lock().expect("env lock");
            Self { keys, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for key in &self.keys {
                std::env::remove_var(key);
            }
        }
    }

    fn tool_call(id: &str, name: &str, arguments: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": arguments}
        })
    }

    fn text_body(text: &str) -> String {
        serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string()
    }

    fn tools_body(calls: Vec<serde_json::Value>, content: &str) -> String {
        serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": content, "tool_calls": calls},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string()
    }

    fn final_text() -> String {
        "The Obscura engine powers headless browsing.\n\n## Findings\n\n- Point one with [t](https://example.com/t).\n".to_owned()
    }

    fn spawn_server(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        let addr = listener.local_addr().expect("listener has an address");
        std::thread::spawn(move || {
            for body in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                read_request(&mut stream);
                let head = format!(
                    "HTTP/1.1 200 x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
            }
        });
        format!("http://{addr}/v1")
    }

    fn read_request(stream: &mut std::net::TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
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
                name.trim()
                    .eq_ignore_ascii_case("content-length")
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

    fn temp_session_out(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "web-agent-research-{tag}-{}-{nanos}.jsonl",
            std::process::id()
        ))
    }

    fn hermetic_request(session_tag: &str) -> ResearchRequest {
        ResearchRequest {
            goal: "What is the Obscura headless browser?".to_owned(),
            size: SynthesisSize::Small,
            max_turns: 8,
            session_id: Some(format!("test-{session_tag}")),
            session_out: Some(temp_session_out(session_tag)),
        }
    }

    fn point_env_at(base_url: &str) {
        std::env::set_var("GATEWAY_BASE_URL", base_url);
        std::env::set_var("GATEWAY_API_KEY", "dummy");
        std::env::remove_var("GATEWAY_MODEL");
    }

    #[tokio::test]
    async fn hermetic_run_prints_synthesis_with_citation() {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        let base_url = spawn_server(vec![
            tools_body(
                vec![tool_call("c1", "search", r#"{"query": "obscura"}"#)],
                "",
            ),
            tools_body(
                vec![tool_call(
                    "c2",
                    "fetch",
                    r#"{"url": "https://example.com/obscura"}"#,
                )],
                "fetching now",
            ),
            text_body(&final_text()),
        ]);
        point_env_at(&base_url);
        let req = hermetic_request("a1");
        let session_out = req.session_out.clone().expect("session out set");
        let response = run_research(req).await.expect("hermetic run must succeed");

        assert!(!response.synthesis.summary.is_empty());
        assert!(
            !response.synthesis.citations.is_empty(),
            "FINAL link must surface as a citation"
        );
        assert_eq!(response.turns_used, 3);

        let printed = render(&response);
        assert!(printed.contains(&response.synthesis.summary));
        assert!(printed.contains("Sources:"));
        assert!(printed.contains("https://example.com/t"));

        let _ = std::fs::remove_file(&session_out);
    }

    #[tokio::test]
    async fn hermetic_response_shape_echoes_session_and_sums_usage() {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        let base_url = spawn_server(vec![
            tools_body(
                vec![tool_call("c1", "search", r#"{"query": "obscura"}"#)],
                "",
            ),
            tools_body(
                vec![tool_call(
                    "c2",
                    "fetch",
                    r#"{"url": "https://example.com/obscura"}"#,
                )],
                "fetching now",
            ),
            text_body(&final_text()),
        ]);
        point_env_at(&base_url);
        let req = hermetic_request("a3");
        let session_out = req.session_out.clone().expect("session out set");
        let response = run_research(req).await.expect("hermetic run must succeed");

        assert_eq!(response.session_id, "test-a3");
        // Live registry returns the same canned stub URL per tool turn,
        // in turn order.
        assert_eq!(
            response.evidence_urls,
            vec![
                "https://example.com/obscura".to_owned(),
                "https://example.com/obscura".to_owned()
            ]
        );
        assert_eq!(response.usage.prompt_tokens, 3);
        assert_eq!(response.usage.completion_tokens, 3);
        assert_eq!(response.usage.total_tokens, 6);

        let content = std::fs::read_to_string(&session_out).expect("session file must exist");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4, "header + 3 turn rows, got {content}");
        let _ = std::fs::remove_file(&session_out);
    }

    #[tokio::test]
    async fn empty_goal_is_not_configured_without_io() {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        std::env::set_var("GATEWAY_API_KEY", "dummy");
        let session_out = temp_session_out("empty-goal");
        let req = ResearchRequest {
            goal: "   ".to_owned(),
            size: SynthesisSize::Small,
            max_turns: 8,
            session_id: Some("test-empty".to_owned()),
            session_out: Some(session_out.clone()),
        };
        let err = run_research(req).await.expect_err("empty goal must fail");
        assert!(
            matches!(err, ResearchError::NotConfigured(_)),
            "empty goal must be NotConfigured, got {err:?}"
        );
        assert!(!session_out.exists(), "NotConfigured must not attempt I/O");
    }

    #[tokio::test]
    async fn zero_max_turns_is_not_configured() {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        std::env::set_var("GATEWAY_API_KEY", "dummy");
        let req = ResearchRequest {
            goal: "What is the Obscura headless browser?".to_owned(),
            size: SynthesisSize::Small,
            max_turns: 0,
            session_id: Some("test-zero".to_owned()),
            session_out: Some(temp_session_out("zero-turns")),
        };
        let err = run_research(req).await.expect_err("max_turns=0 must fail");
        assert!(
            matches!(err, ResearchError::NotConfigured(_)),
            "max_turns=0 must be NotConfigured, got {err:?}"
        );
    }

    #[test]
    fn response_json_shape_is_client_ready() {
        let response = ResearchResponse {
            session_id: "1788825600-1234".to_owned(),
            synthesis: SynthesisDTO {
                size: SynthesisSize::Medium,
                summary: "summary".to_owned(),
                themes: vec![ThemeDTO {
                    title: "T".to_owned(),
                    points: vec!["p".to_owned()],
                    citations: vec![CitationDTO {
                        url: "https://example.com/t".to_owned(),
                        title: Some("t".to_owned()),
                    }],
                }],
                citations: vec![CitationDTO {
                    url: "https://example.com/t".to_owned(),
                    title: Some("t".to_owned()),
                }],
            },
            turns_used: 3,
            evidence_urls: vec!["https://example.com/t".to_owned()],
            usage: UsageDTO {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                reasoning_tokens: 0,
            },
        };
        let value = serde_json::to_value(&response).expect("response serializes");
        assert_eq!(value["session_id"], "1788825600-1234");
        assert_eq!(value["synthesis"]["size"], "medium");
        assert_eq!(value["turns_used"], 3);
        assert_eq!(value["evidence_urls"][0], "https://example.com/t");
        assert_eq!(value["usage"]["total_tokens"], 0);
    }

    #[test]
    fn request_size_parses_lowercase() {
        let req: ResearchRequest =
            serde_json::from_str(r#"{"goal":"g","size":"large","max_turns":8}"#)
                .expect("request deserializes");
        assert_eq!(req.size, SynthesisSize::Large);
        assert_eq!(req.session_id, None);
        let back = serde_json::to_value(&req).expect("request serializes");
        assert_eq!(back["size"], "large");
    }
}
