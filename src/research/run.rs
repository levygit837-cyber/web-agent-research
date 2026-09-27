//! One Research end to end: run the agent loop, synthesize, persist.
//!
//! Deep module behind the public interface (ADR-0006): `run_research()` owns
//! gateway construction, `run_loop` wiring, JSONL append, and the response
//! shape. `src/main.rs` is the only adapter: it prints `Ok` to stdout
//! (`--json` or the human `render`) and maps `Err` to a CLI exit code via
//! `exit_for`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::llm::{Gateway, GatewayConfig, TokenUsage};
use crate::research::agent_loop::{run_loop, LoopBudget, LoopError, LoopInput, ToolRegistry};
use crate::research::session::{append_header, append_turn, SessionHeader, SessionUsage, TurnRow};
use crate::research::synthesis::{Citation, Synthesis, SynthesisSize, ThemeSection};
use crate::web::fetch::{Evidence, Fetcher};
use crate::web::search::tool::Searcher;

/// Default turn budget when the caller omits `max_turns`.
pub fn default_max_turns() -> u32 {
    LoopBudget::default().max_turns
}

fn default_size() -> SynthesisSize {
    SynthesisSize::default()
}

mod size_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::research::synthesis::SynthesisSize;

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

/// Research input, built by `main.rs` from CLI args (also constructed
/// directly in tests).
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
    pub cached_prompt_tokens: u64,
    /// Prompt tokens written to the provider cache (Anthropic
    /// `cache_creation_input_tokens`); 0 where the provider does not report it.
    #[serde(default)]
    pub cache_creation_prompt_tokens: u64,
}

/// Research output; serializes to `--json` stdout, or renders via `render`
/// for human stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResearchResponse {
    pub session_id: String,
    pub synthesis: SynthesisDTO,
    pub turns_used: u32,
    pub evidence_urls: Vec<String>,
    pub usage: UsageDTO,
}

/// Failures of one research run; each maps to a CLI exit code via
/// `exit_for` in `main.rs` (`NotConfigured→2`, `GatewayExhausted→3`,
/// `ToolFailure→4`, `BudgetExhausted→5`, `Io→6`, `SearchBlocked→7`).
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
    /// The run finalized with zero fetched Evidence, no `search` call ever
    /// returned a Hit, and at least one `search` call failed with every
    /// leg `Challenge` (#53): a fully bot-walled run that would otherwise
    /// exit 0 with an empty "I found nothing" Synthesis. Distinct from
    /// `ToolFailure`, which fires inside the loop once repairs are
    /// exhausted; this fires after a FINAL parsed, since the model can
    /// legally answer "no results" without exhausting any repair budget.
    /// Carries the per-engine challenge detail from the last walled call.
    SearchBlocked(String),
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
            Self::SearchBlocked(detail) => {
                write!(f, "search blocked by a bot-detection challenge: {detail}")
            }
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
            cached_prompt_tokens: usage.cached_prompt_tokens,
            cache_creation_prompt_tokens: usage.cache_creation_prompt_tokens,
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
            cached_prompt_tokens: usage.cached_prompt_tokens,
            cache_creation_prompt_tokens: usage.cache_creation_prompt_tokens,
        }
    }
}

fn map_loop_error(err: LoopError) -> ResearchError {
    match err {
        LoopError::NoStart(reason) => ResearchError::NotConfigured(reason),
        LoopError::Gateway(gateway_err) => {
            let message = gateway_err.to_string();
            if matches!(gateway_err, crate::llm::GatewayError::RateLimited { .. }) {
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
    run_research_with(req, ToolRegistry::new(Searcher::new(), Fetcher::new())).await
}

/// [`run_research`] over an injected registry (test seam for hermetic runs).
pub(crate) async fn run_research_with(
    req: ResearchRequest,
    tools: ToolRegistry,
) -> Result<ResearchResponse, ResearchError> {
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
    // Generated before the loop (not after) so it can be sent as
    // `prompt_cache_key` on every turn of this run, keeping cache-routing
    // stable across the whole Session rather than only the persisted rows.
    let session_id = req.session_id.clone().unwrap_or_else(generate_session_id);
    let config = GatewayConfig::from_env()
        .map_err(|err| ResearchError::NotConfigured(err.to_string()))?
        .with_prompt_cache_key(session_id.clone());
    let gateway = Gateway::new(config);
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
    // #53: a FINAL parsed, but every fetched-Evidence source is empty, no
    // `search` call this run ever saw a Hit, and at least one `search` call
    // was fully Challenge-walled. Surface it as a typed failure (exit 7)
    // instead of letting an "I found nothing" Synthesis exit 0 -- the model
    // is legally allowed to answer that way without exhausting any repair
    // budget, so this check runs after the loop, not inside it.
    if let Some(detail) = report.search_blocked {
        if evidence_urls.is_empty() && !report.had_search_hits {
            return Err(ResearchError::SearchBlocked(detail));
        }
    }
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
                item.url.clone().map(|url| {
                    let mut evidence = Evidence::new(url, item.excerpt.clone());
                    evidence.fetch_path = item.fetch_path;
                    evidence
                })
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
    use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

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

    /// Like [`spawn_server`], but also records each request's raw bytes so
    /// wire-body assertions (e.g. `prompt_cache_key`) can inspect exactly
    /// what `run_research_with` sent.
    fn spawn_capturing_server(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        let addr = listener.local_addr().expect("listener has an address");
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_in_thread = Arc::clone(&requests);
        std::thread::spawn(move || {
            for body in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request_capturing(&mut stream);
                requests_in_thread
                    .lock()
                    .expect("requests lock")
                    .push(request);
                let head = format!(
                    "HTTP/1.1 200 x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
            }
        });
        (format!("http://{addr}/v1"), requests)
    }

    fn read_request_capturing(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while let Ok(n) = stream.read(&mut chunk) {
            if n == 0 {
                break;
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
                break;
            };
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            remaining = remaining.saturating_sub(n);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn wire_body_of(request: &str) -> serde_json::Value {
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("captured request must carry a body");
        serde_json::from_str(body).expect("wire body must be JSON")
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

    /// Local web: a page server plus a search stub whose Hits point at it.
    async fn local_web() -> (ToolRegistry, String) {
        use crate::web::search::test_support::{
            ddg_rows, sp_home_form, sp_rows, FanoutStub, StubReply, StubServer,
        };
        let pages = StubServer::serve(|_: &str, _: &str| {
            StubReply::text(
                200,
                &format!(
                    "<html><body><h1>Obscura</h1><p>{}</p></body></html>",
                    "Obscura is a headless browser written in Rust. ".repeat(8)
                ),
            )
        })
        .await;
        let page_url = format!("{}/obscura", pages.base());
        let (search_base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            ddg_rows(&page_url, "Obscura", "headless browser"),
            sp_home_form(),
            sp_rows(&page_url, "Obscura", "headless browser"),
            false,
        ))
        .await;
        let searcher = Searcher::with_bases(
            &format!("{search_base}/html/"),
            &format!("{search_base}/"),
            &format!("{search_base}/sp/search"),
        );
        let fetcher = Fetcher::with_obscura(crate::web::fetch::Obscura::new(
            "/nonexistent/obscura".into(),
            std::time::Duration::from_secs(1),
        ));
        (ToolRegistry::new(searcher, fetcher), page_url)
    }

    /// A search stub where every leg on every provider is bot-wall
    /// Challenge-walled (#53): `searcher.search` always returns
    /// `SearchProviderError::AllFailed { all_challenged: true, .. }`. No
    /// page server: a walled search never reaches fetch.
    async fn walled_web() -> ToolRegistry {
        use crate::web::search::test_support::{sp_home_form, FanoutStub, StubServer};
        let (search_base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
            r#"<div id="anomaly-modal"></div>"#.to_string(),
            sp_home_form(),
            r#"<script id="anubis_challenge" type="application/json">{}</script>"#.to_string(),
            false,
        ))
        .await;
        let searcher = Searcher::with_bases(
            &format!("{search_base}/html/"),
            &format!("{search_base}/"),
            &format!("{search_base}/sp/search"),
        );
        let fetcher = Fetcher::with_obscura(crate::web::fetch::Obscura::new(
            "/nonexistent/obscura".into(),
            std::time::Duration::from_secs(1),
        ));
        ToolRegistry::new(searcher, fetcher)
    }

    #[tokio::test]
    async fn hermetic_run_uses_real_tools_and_persists_fetched_evidence() {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        let (tools, page_url) = local_web().await;
        let final_answer = format!(
            "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust per [Obscura]({page_url}).\n"
        );
        let base_url = spawn_server(vec![
            tools_body(
                vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
                "",
            ),
            tools_body(
                vec![tool_call(
                    "c2",
                    "fetch",
                    &serde_json::json!({ "url": page_url }).to_string(),
                )],
                "fetching now",
            ),
            text_body(&final_answer),
        ]);
        point_env_at(&base_url);
        let req = hermetic_request("a1");
        let session_out = req.session_out.clone().expect("session out set");
        let response = run_research_with(req, tools)
            .await
            .expect("hermetic run must succeed");

        assert_eq!(response.session_id, "test-a1");
        assert_eq!(response.turns_used, 3);
        // Search Hits are not Evidence: only the fetched page is.
        assert_eq!(response.evidence_urls, vec![page_url.clone()]);
        assert!(response
            .synthesis
            .citations
            .iter()
            .any(|citation| citation.url == page_url));
        assert_eq!(response.usage.total_tokens, 6);
        assert!(render(&response).contains(&page_url));

        let content = std::fs::read_to_string(&session_out).expect("session file must exist");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4, "header + 3 turn rows, got {content}");
        assert!(
            lines[2].contains("headless browser written in Rust"),
            "turn 2 must persist fetched markdown: {}",
            lines[2]
        );
        assert!(
            lines[2].contains("\"fetch_path\":\"static\""),
            "fetched Evidence must record which engine produced it: {}",
            lines[2]
        );
        assert!(
            !lines[1].contains("\"source_url\""),
            "search turn has no Evidence"
        );
        let _ = std::fs::remove_file(&session_out);
    }

    #[tokio::test]
    async fn anthropic_format_run_replays_thinking_and_sums_cache_reads() {
        let _guard = EnvGuard::lock(vec![
            "GATEWAY_BASE_URL",
            "GATEWAY_API_KEY",
            "GATEWAY_MODEL",
            "GATEWAY_API_FORMAT",
        ]);
        let (tools, page_url) = local_web().await;
        let final_answer = format!(
            "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust per [Obscura]({page_url}).\n"
        );
        let fetch_turn = serde_json::json!({
            "type": "message",
            "content": [
                {"type": "thinking", "thinking": "fetch it", "signature": "sig-1"},
                {"type": "tool_use", "id": "toolu_1", "name": "fetch", "input": {"url": page_url}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 3,
                      "cache_creation_input_tokens": 4200, "cache_read_input_tokens": 0}
        })
        .to_string();
        let answer_turn = serde_json::json!({
            "type": "message",
            "content": [{"type": "text", "text": final_answer}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 50, "output_tokens": 40,
                      "cache_creation_input_tokens": 60, "cache_read_input_tokens": 4200}
        })
        .to_string();
        let (base_url, requests) = spawn_capturing_server(vec![fetch_turn, answer_turn]);
        point_env_at(&base_url);
        std::env::set_var("GATEWAY_API_FORMAT", "anthropic");
        let req = hermetic_request("anthropic");
        let session_out = req.session_out.clone().expect("session out set");
        let response = run_research_with(req, tools)
            .await
            .expect("hermetic anthropic run must succeed");

        assert_eq!(response.turns_used, 2);
        assert_eq!(response.evidence_urls, vec![page_url.clone()]);
        assert_eq!(response.usage.cached_prompt_tokens, 4200);
        assert_eq!(response.usage.cache_creation_prompt_tokens, 4260);
        assert_eq!(response.usage.prompt_tokens, 4205 + 4310);

        let requests = requests.lock().expect("requests lock");
        assert!(
            requests[0].starts_with("POST /v1/messages "),
            "{}",
            requests[0]
        );
        let second = wire_body_of(&requests[1]);
        let messages = second["messages"].as_array().expect("messages");
        let assistant = &messages[messages.len() - 2];
        assert_eq!(
            assistant["content"][0],
            serde_json::json!({"type": "thinking", "thinking": "fetch it", "signature": "sig-1"}),
            "thinking block must be replayed verbatim first: {second}"
        );
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        let results = &messages[messages.len() - 1];
        assert_eq!(results["content"][0]["type"], "tool_result");
        assert_eq!(results["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(
            results["content"][0]["cache_control"]["type"], "ephemeral",
            "the newest block carries the moving breakpoint"
        );
        let _ = std::fs::remove_file(&session_out);
    }

    #[tokio::test]
    async fn cached_prompt_tokens_sum_across_turns_from_both_wire_shapes() {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        let (tools, page_url) = local_web().await;
        // Turn 1 reports the OpenAI shape, turn 2 the Anthropic/LiteLLM shape.
        let with_usage = |mut body: serde_json::Value, usage: serde_json::Value| {
            body["usage"] = usage;
            body.to_string()
        };
        let search_turn: serde_json::Value = serde_json::from_str(&tools_body(
            vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
            "",
        ))
        .expect("json");
        let final_turn: serde_json::Value = serde_json::from_str(&text_body(&format!(
            "Obscura is a Rust headless browser.\n\n## Findings\n\n- See [Obscura]({page_url}).\n"
        )))
        .expect("json");
        let base_url = spawn_server(vec![
            with_usage(
                search_turn,
                serde_json::json!({
                    "prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                    "prompt_tokens_details": {"cached_tokens": 64}
                }),
            ),
            with_usage(
                final_turn,
                serde_json::json!({
                    "prompt_tokens": 200, "completion_tokens": 7, "total_tokens": 207,
                    "cache_read_input_tokens": 96
                }),
            ),
        ]);
        point_env_at(&base_url);
        let req = hermetic_request("cache-sum");
        let session_out = req.session_out.clone().expect("session out set");
        let response = run_research_with(req, tools)
            .await
            .expect("hermetic run must succeed");

        assert_eq!(response.turns_used, 2);
        assert_eq!(response.usage.cached_prompt_tokens, 160);
        assert_eq!(response.usage.prompt_tokens, 300);
        let content = std::fs::read_to_string(&session_out).expect("session file must exist");
        let last = content.lines().last().expect("final turn row");
        assert!(
            last.contains("\"cached_prompt_tokens\":160"),
            "final Session row must carry the run total: {last}"
        );
        let _ = std::fs::remove_file(&session_out);
    }
    #[tokio::test]
    async fn prompt_cache_key_is_the_session_id_generated_before_the_loop_and_stable_across_turns()
    {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        let (tools, page_url) = local_web().await;
        let final_answer = format!(
            "Obscura is a Rust headless browser.\n\n## Findings\n\n- Written in Rust per [Obscura]({page_url}).\n"
        );
        let (base_url, requests) = spawn_capturing_server(vec![
            tools_body(
                vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
                "",
            ),
            tools_body(
                vec![tool_call(
                    "c2",
                    "fetch",
                    &serde_json::json!({ "url": page_url }).to_string(),
                )],
                "fetching now",
            ),
            text_body(&final_answer),
        ]);
        point_env_at(&base_url);
        // No explicit `session_id`: forces `generate_session_id()`, which
        // must run BEFORE the loop so every turn's wire body carries it.
        let req = ResearchRequest {
            goal: "What is the Obscura headless browser?".to_owned(),
            size: SynthesisSize::Small,
            max_turns: 8,
            session_id: None,
            session_out: Some(temp_session_out("cache-key-stable")),
        };
        let session_out = req.session_out.clone().expect("session out set");
        let response = run_research_with(req, tools)
            .await
            .expect("hermetic run must succeed");

        assert_eq!(response.turns_used, 3);
        assert!(
            !response.session_id.is_empty(),
            "auto-generated session_id must be non-empty"
        );

        let bodies: Vec<serde_json::Value> = requests
            .lock()
            .expect("requests lock")
            .iter()
            .map(|r| wire_body_of(r))
            .collect();
        assert_eq!(bodies.len(), 3, "must have captured all 3 turns");
        for (turn, body) in bodies.iter().enumerate() {
            assert_eq!(
                body["prompt_cache_key"],
                serde_json::json!(response.session_id),
                "turn {} prompt_cache_key must equal the run's session_id, got {body}",
                turn + 1
            );
        }

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
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
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

    /// Acceptance (#53): a FINAL answer with zero fetched Evidence, when
    /// every search call this run was fully Challenge-walled and no search
    /// call ever returned a Hit, must surface as `ResearchError::SearchBlocked`
    /// (exit 7 in `main.rs`), never a silent exit-0 "I found nothing"
    /// Synthesis (the bug reported in #53).
    #[tokio::test]
    async fn fully_walled_search_surfaces_search_blocked_not_empty_synthesis() {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        let tools = walled_web().await;
        let base_url = spawn_server(vec![
            tools_body(
                vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
                "",
            ),
            text_body("I found nothing on this topic.\n\n## Findings\n\n- No sources found.\n"),
        ]);
        point_env_at(&base_url);
        let req = hermetic_request("walled");
        let session_out = req.session_out.clone().expect("session out set");
        let err = run_research_with(req, tools)
            .await
            .expect_err("a fully walled search must not exit as a successful empty Synthesis");
        match &err {
            ResearchError::SearchBlocked(detail) => {
                assert!(
                    detail.contains("startpage") && detail.contains("duckduckgo"),
                    "detail must carry both engines' challenge messages: {detail}"
                );
            }
            other => panic!("expected SearchBlocked, got {other:?}"),
        }
        let _ = std::fs::remove_file(&session_out);
    }

    /// A search call returning at least one Hit, even if the model still
    /// answers "no results" in its FINAL text, must NOT be reclassified as
    /// `SearchBlocked`: the search itself worked, the model just chose not
    /// to fetch anything.
    #[tokio::test]
    async fn hits_without_fetch_is_not_search_blocked() {
        let _guard = EnvGuard::lock(vec!["GATEWAY_BASE_URL", "GATEWAY_API_KEY", "GATEWAY_MODEL"]);
        let (tools, _page_url) = local_web().await;
        let base_url = spawn_server(vec![
            tools_body(
                vec![tool_call("c1", "search", r#"{"queries": ["obscura"]}"#)],
                "",
            ),
            text_body("No relevant sources found.\n\n## Findings\n\n- Nothing conclusive.\n"),
        ]);
        point_env_at(&base_url);
        let req = hermetic_request("hits-no-fetch");
        let session_out = req.session_out.clone().expect("session out set");
        let response = run_research_with(req, tools)
            .await
            .expect("a search that returned Hits must finalize as Ok, not SearchBlocked");
        assert!(response.evidence_urls.is_empty());
        let _ = std::fs::remove_file(&session_out);
    }
}
