//! Agent loop runner with turn budget and stop condition.
//!
//! One function is the external seam: [`run_loop`] borrows the gateway and
//! the tool registry, offers `allowed_tools` with `ToolChoice::Auto` (plain
//! `chat()` for zero-tool runs), feeds tool results back as text-transcript
//! observations, and repeats until the first tool-free non-empty answer,
//! which becomes the typed [`Synthesis`]. Gateway errors reaching the loop
//! are final for that turn (the gateway owns retries); tool and dispatch
//! failures are model-visible observations bounded by `max_repairs`.

use std::time::Duration;

use crate::shared::agent_loop::context::{self, HistoryEntry};
use crate::shared::agent_loop::registry::{ToolRegistry, ToolResult};
use crate::shared::agent_loop::synthesis::parse_answer;
use crate::shared::gateway::{Gateway, GatewayError, RequestedToolCall, TokenUsage, ToolChoice};
use crate::shared::prompts::execute::build_system_prompt;
use crate::shared::types::synthesis::{Synthesis, SynthesisSize};

#[derive(Debug, Clone)]
pub struct LoopInput {
    /// Research goal, user words. Must be non-empty after trim.
    pub goal: String,
    /// Requested synthesis size; echoed into `Synthesis::size`.
    pub size: SynthesisSize,
    /// Tool names the run may offer, e.g. `["search", "fetch"]`.
    /// Unknown names are a NoStart (fail before turn 1, not mid-run).
    /// Empty is legal: a zero-tool run answers via plain `chat()`.
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LoopBudget {
    /// Total gateway calls allowed.
    pub max_turns: u32,
    /// Consecutive failure turns before abort.
    pub max_repairs: u32,
    /// Per-tool-call deadline, applied by the runner.
    pub tool_timeout: Duration,
    /// Per-evidence-text cap before the truncation marker.
    pub max_evidence_chars: usize,
    /// Total assembled-char cap before oldest-pair dropping.
    pub max_context_chars: usize,
    /// Wire-call cap per turn.
    pub max_tools_per_turn: usize,
}

impl Default for LoopBudget {
    fn default() -> Self {
        Self {
            max_turns: 8,
            max_repairs: 2,
            tool_timeout: Duration::from_secs(45),
            max_evidence_chars: 4_000,
            max_context_chars: 24_000,
            max_tools_per_turn: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolEvidence {
    /// 1-based turn that produced it.
    pub turn: u32,
    /// Wire call id (`""` when the model omitted it).
    pub id: String,
    /// Executed tool name.
    pub tool: String,
    /// Capped at `max_evidence_chars` (marker when cut).
    pub excerpt: String,
    /// `Some` when the tool yields a source URL.
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunReport {
    pub synthesis: Synthesis,
    /// Gateway calls made, `1..=max_turns`.
    pub turns_used: u32,
    /// Total failure turns consumed in the run.
    pub failures_used: u32,
    /// Document order (turn ascending, then wire order).
    pub evidence: Vec<ToolEvidence>,
    /// Saturating sum of per-turn usage.
    pub usage: TokenUsage,
}

#[derive(Debug)]
pub enum LoopError {
    /// Pre-flight: empty goal, unknown `allowed_tools` entry, missing key,
    /// `max_turns == 0`, `max_context_chars == 0`. No I/O was attempted.
    NoStart(String),
    /// Gateway `chat_with_tools`/`chat` returned `Err` (retries exhausted).
    Gateway(GatewayError),
    /// Consecutive dispatch-kind failures exceeded `max_repairs`.
    /// Carries the last failure's reason.
    InvalidToolCall { reason: String },
    /// Consecutive tool-execution failures exceeded `max_repairs`.
    ToolFailed { reason: String },
    /// `max_turns` consumed with no FINAL parsed.
    BudgetExhausted { turns: u32 },
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStart(reason) => write!(f, "cannot start research: {reason}"),
            Self::Gateway(err) => write!(f, "gateway turn failed: {err}"),
            Self::InvalidToolCall { reason } => {
                write!(f, "invalid tool call, repairs exhausted: {reason}")
            }
            Self::ToolFailed { reason } => {
                write!(f, "tool execution failed, repairs exhausted: {reason}")
            }
            Self::BudgetExhausted { turns } => {
                write!(
                    f,
                    "turn budget exhausted after {turns} turns with no final answer"
                )
            }
        }
    }
}

impl std::error::Error for LoopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gateway(err) => Some(err),
            _ => None,
        }
    }
}

impl From<GatewayError> for LoopError {
    fn from(err: GatewayError) -> Self {
        Self::Gateway(err)
    }
}

/// Test seam: observe context growth without exposing history publicly.
#[allow(dead_code)]
pub(crate) fn history_len(history: &[HistoryEntry]) -> usize {
    history.len()
}

fn allowed_roster(tools: &ToolRegistry, allowed: &[String]) -> Vec<(&'static str, &'static str)> {
    tools
        .tool_purposes()
        .into_iter()
        .filter(|(name, _)| allowed.iter().any(|entry| entry == name))
        .collect()
}

fn roster_footer(tools: &ToolRegistry, allowed: &[String]) -> String {
    allowed_roster(tools, allowed)
        .iter()
        .map(|(name, purpose)| format!("{name} — {purpose}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn empty_answer_observation(tools: &ToolRegistry, allowed: &[String]) -> String {
    format!(
        "TOOL ERROR: empty answer — use the offered tools to gather evidence, or answer directly when evidence suffices.\nAvailable tools:\n{}",
        roster_footer(tools, allowed)
    )
}

fn display_id(id: &str) -> &str {
    if id.is_empty() {
        "<no-id>"
    } else {
        id
    }
}

fn is_dispatch_failure(result: &ToolResult) -> bool {
    match result {
        ToolResult::Failed { reason, .. } => {
            reason.contains("unknown tool")
                || reason.contains("not valid JSON")
                || reason.contains("invalid args")
        }
        _ => false,
    }
}

fn cap_excerpt(rendered: &str, budget: &LoopBudget) -> String {
    context::cap_evidence(rendered, budget.max_evidence_chars)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    Dispatch,
    Execution,
}

struct TurnOutcome {
    observation: String,
    evidence: Vec<ToolEvidence>,
    had_success: bool,
    failure: Option<FailureKind>,
    failure_reason: String,
}

async fn dispatch_turn(
    turn: u32,
    calls: &[RequestedToolCall],
    tools: &ToolRegistry,
    allowed: &[String],
    budget: &LoopBudget,
) -> TurnOutcome {
    let cap = budget.max_tools_per_turn.max(1);
    let considered: Vec<&RequestedToolCall> = calls.iter().take(cap).collect();
    let dropped = calls.len().saturating_sub(considered.len());

    let mut rows: Vec<String> = Vec::new();
    let mut evidence: Vec<ToolEvidence> = Vec::new();
    let mut had_success = false;
    let mut failure: Option<FailureKind> = None;
    let mut failure_reason = String::new();

    for call in considered {
        let is_allowed = allowed.iter().any(|name| name == &call.name);
        if !is_allowed {
            let reason = format!(
                "unknown tool '{}' (available: {})",
                call.name,
                allowed.join(", ")
            );
            rows.push(cap_excerpt(&format!("TOOL ERROR: {reason}"), budget));
            failure = Some(FailureKind::Dispatch);
            failure_reason = reason;
            break;
        }
        let executed = tokio::time::timeout(budget.tool_timeout, tools.execute(call)).await;
        match executed {
            Err(_) => {
                let reason = format!("timeout after {}s", budget.tool_timeout.as_secs());
                rows.push(cap_excerpt(
                    &format!(
                        "TOOL RESULTS:\n[{} {} FAILED: {reason}]",
                        display_id(&call.id),
                        call.name
                    ),
                    budget,
                ));
                evidence.push(ToolEvidence {
                    turn,
                    id: call.id.clone(),
                    tool: call.name.clone(),
                    excerpt: cap_excerpt(&format!("TIMEOUT: {reason}"), budget),
                    url: None,
                });
                failure = Some(FailureKind::Execution);
                failure_reason = reason;
            }
            Ok(result) => {
                let rendered = result.render();
                let dispatch_failure = is_dispatch_failure(&result);
                let excerpt = cap_excerpt(&rendered, budget);
                if result.is_success() {
                    had_success = true;
                    rows.push(cap_excerpt(
                        &format!(
                            "TOOL RESULTS:\n[{} {}({})]: {rendered}",
                            display_id(&call.id),
                            call.name,
                            call.arguments
                        ),
                        budget,
                    ));
                    evidence.push(ToolEvidence {
                        turn,
                        id: call.id.clone(),
                        tool: call.name.clone(),
                        excerpt,
                        url: result.url(),
                    });
                } else if dispatch_failure {
                    rows.push(cap_excerpt(&format!("TOOL ERROR: {rendered}"), budget));
                    failure = Some(FailureKind::Dispatch);
                    failure_reason = rendered;
                    break;
                } else {
                    rows.push(cap_excerpt(
                        &format!(
                            "TOOL RESULTS:\n[{} {} FAILED: {rendered}]",
                            display_id(&call.id),
                            call.name
                        ),
                        budget,
                    ));
                    evidence.push(ToolEvidence {
                        turn,
                        id: call.id.clone(),
                        tool: call.name.clone(),
                        excerpt,
                        url: None,
                    });
                    failure = Some(FailureKind::Execution);
                    failure_reason = rendered;
                }
            }
        }
    }

    let mut observation = rows.join("\n");
    if dropped > 0 {
        let note = format!("[+{dropped} call(s) dropped: per-turn cap {cap}]");
        if observation.is_empty() {
            observation = format!("TOOL RESULTS:\n{note}");
        } else {
            observation.push('\n');
            observation.push_str(&note);
        }
    }
    if failure == Some(FailureKind::Dispatch) {
        let footer = roster_footer(tools, allowed);
        observation.push_str(&format!("\nAvailable tools:\n{footer}"));
    } else if failure.is_none() && !observation.is_empty() && !observation.contains("TOOL RESULTS:")
    {
        observation = format!("TOOL RESULTS:\n{observation}");
    }
    TurnOutcome {
        observation,
        evidence,
        had_success,
        failure,
        failure_reason,
    }
}

/// Run the multi-turn research loop until the first tool-free answer.
///
/// Borrows the gateway and registry; exactly one gateway call per turn.
/// See the module docs and SPEC §4 for the full contract.
pub async fn run_loop(
    gateway: &Gateway,
    tools: &ToolRegistry,
    input: &LoopInput,
    budget: &LoopBudget,
) -> Result<RunReport, LoopError> {
    if input.goal.trim().is_empty() {
        return Err(LoopError::NoStart("research goal is empty".to_owned()));
    }
    let known = tools.tool_names();
    for allowed in &input.allowed_tools {
        if !known.iter().any(|name| *name == allowed) {
            return Err(LoopError::NoStart(format!("unknown tool: {allowed}")));
        }
    }
    if budget.max_turns < 1 {
        return Err(LoopError::NoStart("max_turns must be >= 1".to_owned()));
    }
    if budget.max_context_chars < 1 {
        return Err(LoopError::NoStart(
            "max_context_chars must be >= 1".to_owned(),
        ));
    }

    let roster: Vec<(&str, &str)> = allowed_roster(tools, &input.allowed_tools);
    let system_prompt = build_system_prompt(&roster, input.size);
    let defs = tools.tool_defs(&input.allowed_tools);

    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut evidence: Vec<ToolEvidence> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut consecutive_failures: u32 = 0;
    let mut failures_used: u32 = 0;

    for turn in 1..=budget.max_turns {
        let messages = context::assemble(&system_prompt, input, &history, budget);
        let reply = if defs.is_empty() {
            gateway.chat(&messages).await
        } else {
            gateway
                .chat_with_tools(&messages, &defs, Some(ToolChoice::Auto))
                .await
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(GatewayError::MissingApiKey) if turn == 1 => {
                return Err(LoopError::NoStart("GATEWAY_API_KEY is not set".to_owned()));
            }
            Err(err) => return Err(LoopError::Gateway(err)),
        };
        usage.prompt_tokens = usage
            .prompt_tokens
            .saturating_add(reply.usage.prompt_tokens);
        usage.completion_tokens = usage
            .completion_tokens
            .saturating_add(reply.usage.completion_tokens);
        usage.total_tokens = usage.total_tokens.saturating_add(reply.usage.total_tokens);
        usage.reasoning_tokens = usage
            .reasoning_tokens
            .saturating_add(reply.usage.reasoning_tokens);
        if reply.finish_reason.as_deref() == Some("length") {
            tracing::warn!("agent loop turn {turn} hit the model length limit; continuing");
        }

        if reply.tool_calls.is_empty() {
            match parse_answer(&reply.output, input.size) {
                Ok(synthesis) => {
                    if !reply.output.is_empty() {
                        history.push(HistoryEntry::Assistant(reply.output));
                    }
                    return Ok(RunReport {
                        synthesis,
                        turns_used: turn,
                        failures_used,
                        evidence,
                        usage,
                    });
                }
                Err(_) => {
                    consecutive_failures += 1;
                    failures_used += 1;
                    if consecutive_failures > budget.max_repairs {
                        return Err(LoopError::InvalidToolCall {
                            reason: "empty answer".to_owned(),
                        });
                    }
                    let observation = empty_answer_observation(tools, &input.allowed_tools);
                    if !reply.output.is_empty() {
                        history.push(HistoryEntry::Assistant(reply.output.clone()));
                    }
                    history.push(HistoryEntry::Observation(observation));
                    if turn == budget.max_turns {
                        return Err(LoopError::BudgetExhausted { turns: turn });
                    }
                    continue;
                }
            }
        }

        let outcome =
            dispatch_turn(turn, &reply.tool_calls, tools, &input.allowed_tools, budget).await;
        evidence.extend(outcome.evidence);
        if !reply.output.is_empty() {
            history.push(HistoryEntry::Assistant(reply.output.clone()));
        }
        history.push(HistoryEntry::Observation(outcome.observation));
        if outcome.had_success {
            consecutive_failures = 0;
        }
        if let Some(kind) = outcome.failure {
            if !outcome.had_success {
                consecutive_failures += 1;
            }
            failures_used += 1;
            let reason = outcome.failure_reason.clone();
            if consecutive_failures > budget.max_repairs {
                return match kind {
                    FailureKind::Dispatch => Err(LoopError::InvalidToolCall { reason }),
                    FailureKind::Execution => Err(LoopError::ToolFailed { reason }),
                };
            }
        }

        if turn == budget.max_turns {
            return Err(LoopError::BudgetExhausted { turns: turn });
        }
    }

    Err(LoopError::BudgetExhausted {
        turns: budget.max_turns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::shared::gateway::GatewayConfig;

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

    fn empty_body() -> String {
        serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": ""},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string()
    }

    fn refusal_body(text: &str) -> String {
        serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "", "refusal": text},
                "finish_reason": "stop"
            }]
        })
        .to_string()
    }

    struct Double {
        base_url: String,
        bodies: Arc<Mutex<Vec<String>>>,
    }

    fn spawn_double(responses: Vec<(u16, String)>) -> Double {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        let addr = listener.local_addr().expect("listener has an address");
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_in_thread = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request(&mut stream);
                bodies_in_thread.lock().expect("bodies lock").push(request);
                let head = format!(
                    "HTTP/1.1 {status} x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
            }
        });
        Double {
            base_url: format!("http://{addr}"),
            bodies,
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
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

    fn gateway_at(base_url: &str) -> Gateway {
        Gateway::with_client(
            GatewayConfig::new(base_url.to_owned(), "test-key".to_owned(), "m".to_owned())
                .with_max_attempts(1),
            reqwest::Client::new(),
        )
    }

    fn stub_tools(results: Vec<ToolResult>) -> ToolRegistry {
        ToolRegistry::stub(VecDeque::from(results))
    }

    fn canned_search() -> ToolResult {
        ToolResult::Search {
            hits: vec![crate::shared::agent_loop::registry::SearchHit {
                title: "T".to_owned(),
                url: "https://example.com/t".to_owned(),
                snippet: "S".to_owned(),
            }],
        }
    }

    fn canned_fetch() -> ToolResult {
        ToolResult::Fetch {
            page: crate::shared::agent_loop::registry::FetchedPage {
                url: "https://example.com/t".to_owned(),
                title: "T".to_owned(),
                markdown: "Body".to_owned(),
            },
        }
    }

    fn loop_input(allowed: &[&str]) -> LoopInput {
        LoopInput {
            goal: "What is the Obscura engine?".to_owned(),
            size: SynthesisSize::Medium,
            allowed_tools: allowed.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    fn final_answer() -> String {
        "Summary with [evidence](https://example.com/t).\n\n## Findings\n\n- Point one.\n"
            .to_owned()
    }

    #[tokio::test]
    async fn three_turn_run_to_final() {
        let double = spawn_double(vec![
            (
                200,
                tools_body(
                    vec![tool_call("c1", "search", r#"{"query": "obscura"}"#)],
                    "",
                ),
            ),
            (
                200,
                tools_body(
                    vec![tool_call(
                        "c2",
                        "fetch",
                        r#"{"url": "https://example.com/t"}"#,
                    )],
                    "fetching now",
                ),
            ),
            (200, text_body(&final_answer())),
        ]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![canned_search(), canned_fetch()]),
            &loop_input(&["search", "fetch"]),
            &LoopBudget::default(),
        )
        .await
        .expect("three-turn run must finalize");
        assert_eq!(report.turns_used, 3);
        assert!(!report.synthesis.summary.is_empty());
        assert_eq!(report.synthesis.size, SynthesisSize::Medium);
        assert!(
            report
                .synthesis
                .citations
                .iter()
                .any(|citation| citation.url == "https://example.com/t"),
            "citations must contain the stub URL"
        );
        assert_eq!(report.evidence.len(), 2);
        assert_eq!(report.evidence[0].turn, 1);
        assert_eq!(report.evidence[1].turn, 2);
        assert_eq!(report.failures_used, 0);
        assert_eq!(report.usage.total_tokens, 6);
    }

    #[tokio::test]
    async fn mixed_text_plus_calls_continues() {
        let double = spawn_double(vec![
            (
                200,
                tools_body(
                    vec![tool_call("c1", "search", r#"{"query": "obscura"}"#)],
                    "thinking out loud",
                ),
            ),
            (200, text_body(&final_answer())),
        ]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![canned_search()]),
            &loop_input(&["search", "fetch"]),
            &LoopBudget::default(),
        )
        .await
        .expect("mixed turn must continue, not finalize");
        assert_eq!(report.turns_used, 2);
        assert!(!report.synthesis.summary.is_empty());
    }

    #[tokio::test]
    async fn schema_injection_sends_tools_and_auto_choice() {
        let double = spawn_double(vec![(200, text_body(&final_answer()))]);
        run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search", "fetch"]),
            &LoopBudget::default(),
        )
        .await
        .expect("zero-call run must finalize");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let bodies = double.bodies.lock().expect("bodies lock");
        assert_eq!(bodies.len(), 1);
        let (_, body) = bodies[0]
            .split_once("\r\n\r\n")
            .expect("recorded request must carry a JSON body");
        let wire: serde_json::Value = serde_json::from_str(body).expect("wire body must be JSON");
        let tools = wire
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["function"]["name"], serde_json::json!("search"));
        assert_eq!(tools[1]["function"]["name"], serde_json::json!("fetch"));
        for tool in tools {
            assert_eq!(tool["type"], serde_json::json!("function"));
            assert_eq!(
                tool["function"]["parameters"]["type"],
                serde_json::json!("object")
            );
        }
        let search = tools
            .iter()
            .find(|t| t["function"]["name"] == "search")
            .unwrap();
        assert!(search["function"]["parameters"]["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("query")));
        let fetch = tools
            .iter()
            .find(|t| t["function"]["name"] == "fetch")
            .unwrap();
        assert!(fetch["function"]["parameters"]["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("url")));
        assert_eq!(wire.get("tool_choice"), Some(&serde_json::json!("auto")));
    }

    #[tokio::test]
    async fn zero_tool_run_uses_plain_chat_shape() {
        let double = spawn_double(vec![(200, text_body(&final_answer()))]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&[]),
            &LoopBudget::default(),
        )
        .await
        .expect("zero-tool run must finalize");
        assert_eq!(report.turns_used, 1);
        let bodies = double.bodies.lock().expect("bodies lock");
        let (_, body) = bodies[0]
            .split_once("\r\n\r\n")
            .expect("recorded request must carry a JSON body");
        let wire: serde_json::Value = serde_json::from_str(body).expect("wire body must be JSON");
        assert!(wire.get("tools").is_none(), "plain chat must omit tools");
        assert!(
            wire.get("tool_choice").is_none(),
            "plain chat must omit tool_choice"
        );
    }

    #[tokio::test]
    async fn over_cap_calls_note_dropped_excess() {
        let calls = vec![
            tool_call("c1", "search", r#"{"query": "a"}"#),
            tool_call("c2", "search", r#"{"query": "b"}"#),
            tool_call("c3", "search", r#"{"query": "c"}"#),
            tool_call("c4", "search", r#"{"query": "d"}"#),
        ];
        let double = spawn_double(vec![
            (200, tools_body(calls, "")),
            (200, text_body(&final_answer())),
        ]);
        let budget = LoopBudget {
            max_tools_per_turn: 3,
            ..LoopBudget::default()
        };
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![canned_search(), canned_search(), canned_search()]),
            &loop_input(&["search"]),
            &budget,
        )
        .await
        .expect("over-cap turn must still finalize");
        assert_eq!(report.turns_used, 2);
        assert_eq!(report.evidence.len(), 3);
    }

    #[tokio::test]
    async fn rejects_empty_goal() {
        let double = spawn_double(vec![]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &LoopInput {
                goal: "   ".to_owned(),
                size: SynthesisSize::Medium,
                allowed_tools: vec![],
            },
            &LoopBudget::default(),
        )
        .await
        .expect_err("empty goal must not start");
        assert!(matches!(err, LoopError::NoStart(_)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            double.bodies.lock().expect("bodies lock").is_empty(),
            "no I/O on NoStart"
        );
    }

    #[tokio::test]
    async fn unknown_tool_is_no_start() {
        let double = spawn_double(vec![]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["nope"]),
            &LoopBudget::default(),
        )
        .await
        .expect_err("unknown allowed tool must not start");
        match err {
            LoopError::NoStart(reason) => assert!(reason.contains("unknown tool: nope")),
            other => panic!("expected NoStart, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn zero_turns_and_zero_context_are_no_start() {
        let double = spawn_double(vec![]);
        let gateway = gateway_at(&double.base_url);
        let tools = stub_tools(vec![]);
        let err = run_loop(
            &gateway,
            &tools,
            &loop_input(&[]),
            &LoopBudget {
                max_turns: 0,
                ..LoopBudget::default()
            },
        )
        .await
        .expect_err("max_turns == 0 must not start");
        assert!(matches!(err, LoopError::NoStart(_)));
        let err = run_loop(
            &gateway,
            &tools,
            &loop_input(&[]),
            &LoopBudget {
                max_context_chars: 0,
                ..LoopBudget::default()
            },
        )
        .await
        .expect_err("max_context_chars == 0 must not start");
        assert!(matches!(err, LoopError::NoStart(_)));
    }

    #[tokio::test]
    async fn no_start_without_key() {
        let gateway = Gateway::new(GatewayConfig::new(
            "http://127.0.0.1:1".to_owned(),
            String::new(),
            "m".to_owned(),
        ));
        let err = run_loop(
            &gateway,
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect_err("missing key must map to NoStart");
        assert!(matches!(err, LoopError::NoStart(_)));
    }

    #[tokio::test]
    async fn auth_aborts() {
        let double = spawn_double(vec![(401, "{}".to_owned())]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect_err("auth must abort");
        assert!(matches!(err, LoopError::Gateway(GatewayError::Auth)));
    }

    #[tokio::test]
    async fn retryable_exhausted_aborts() {
        let double = spawn_double(vec![(500, "{}".to_owned())]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect_err("exhausted 500 must abort");
        assert!(matches!(
            err,
            LoopError::Gateway(GatewayError::Server { status: 500 })
        ));
    }

    #[tokio::test]
    async fn client_error_aborts() {
        let double = spawn_double(vec![(404, "{}".to_owned())]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect_err("4xx must abort");
        assert!(matches!(
            err,
            LoopError::Gateway(GatewayError::Client { status: 404 })
        ));
    }

    #[tokio::test]
    async fn refusal_aborts() {
        let double = spawn_double(vec![(200, refusal_body("nope"))]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect_err("refusal must abort");
        assert!(matches!(err, LoopError::Gateway(GatewayError::Refused(_))));
    }

    #[tokio::test]
    async fn refusal_with_tools_aborts() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "refusal": "nope",
                    "tool_calls": [tool_call("c1", "search", "{}")]
                },
                "finish_reason": "stop"
            }]
        })
        .to_string();
        let double = spawn_double(vec![(200, body)]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect_err("refusal must win over tool calls");
        assert!(matches!(err, LoopError::Gateway(GatewayError::Refused(_))));
    }

    #[tokio::test]
    async fn unknown_tool_repairs_then_finalizes() {
        let double = spawn_double(vec![
            (200, tools_body(vec![tool_call("c1", "nope", "{}")], "")),
            (200, text_body(&final_answer())),
        ]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect("one unknown tool must repair");
        assert_eq!(report.turns_used, 2);
        assert_eq!(report.failures_used, 1);
    }

    #[tokio::test]
    async fn unknown_tool_aborts_after_repairs() {
        let repeated = tools_body(vec![tool_call("c1", "nope", "{}")], "");
        let double = spawn_double(vec![
            (200, repeated.clone()),
            (200, repeated.clone()),
            (200, repeated),
        ]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget {
                max_repairs: 2,
                ..LoopBudget::default()
            },
        )
        .await
        .expect_err("unknown-tool streak must abort");
        match err {
            LoopError::InvalidToolCall { reason } => assert!(reason.contains("unknown tool")),
            other => panic!("expected InvalidToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_arguments_json_repairs() {
        let double = spawn_double(vec![
            (
                200,
                tools_body(vec![tool_call("c1", "search", "{bad json")], ""),
            ),
            (200, text_body(&final_answer())),
        ]);
        let tools = ToolRegistry::live();
        let report = run_loop(
            &gateway_at(&double.base_url),
            &tools,
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect("bad JSON must repair");
        assert_eq!(report.turns_used, 2);
        assert_eq!(report.failures_used, 1);
    }

    #[tokio::test]
    async fn invalid_args_repair_then_abort() {
        let bad = tools_body(vec![tool_call("c1", "search", r#"{"query": ""}"#)], "");
        let double = spawn_double(vec![(200, bad.clone()), (200, bad)]);
        let tools = ToolRegistry::live();
        let err = run_loop(
            &gateway_at(&double.base_url),
            &tools,
            &loop_input(&["search"]),
            &LoopBudget {
                max_repairs: 1,
                ..LoopBudget::default()
            },
        )
        .await
        .expect_err("invalid-args streak must abort");
        match err {
            LoopError::InvalidToolCall { reason } => assert!(reason.contains("invalid args")),
            other => panic!("expected InvalidToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_error_is_observation_then_repair() {
        let double = spawn_double(vec![
            (200, tools_body(vec![tool_call("c1", "search", "{}")], "")),
            (200, text_body(&final_answer())),
        ]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![ToolResult::Failed {
                tool: "search".to_owned(),
                reason: "boom".to_owned(),
            }]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect("executor failure must repair");
        assert_eq!(report.turns_used, 2);
        assert_eq!(report.failures_used, 1);
        assert!(report.evidence.iter().all(|item| item.tool == "search"));
    }

    #[tokio::test]
    async fn repeated_tool_failure_aborts() {
        let failing = tools_body(vec![tool_call("c1", "search", "{}")], "");
        let double = spawn_double(vec![
            (200, failing.clone()),
            (200, failing.clone()),
            (200, failing),
        ]);
        let failing_tools = || {
            ToolRegistry::stub(VecDeque::from([
                ToolResult::Failed {
                    tool: "search".to_owned(),
                    reason: "boom".to_owned(),
                },
                ToolResult::Failed {
                    tool: "search".to_owned(),
                    reason: "boom".to_owned(),
                },
                ToolResult::Failed {
                    tool: "search".to_owned(),
                    reason: "boom".to_owned(),
                },
            ]))
        };
        let err = run_loop(
            &gateway_at(&double.base_url),
            &failing_tools(),
            &loop_input(&["search"]),
            &LoopBudget {
                max_repairs: 2,
                ..LoopBudget::default()
            },
        )
        .await
        .expect_err("execution streak must abort");
        match err {
            LoopError::ToolFailed { reason } => assert!(reason.contains("boom")),
            other => panic!("expected ToolFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_timeout_is_observation_then_repair() {
        let double = spawn_double(vec![
            (
                200,
                tools_body(vec![tool_call("c1", "search", r#"{"query": "a"}"#)], ""),
            ),
            (200, text_body(&final_answer())),
        ]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![ToolResult::Hang]),
            &loop_input(&["search"]),
            &LoopBudget {
                tool_timeout: Duration::from_millis(50),
                ..LoopBudget::default()
            },
        )
        .await
        .expect("timeout must surface as a repairable observation");
        assert_eq!(report.turns_used, 2);
        assert_eq!(report.failures_used, 1);
        assert!(report.evidence.iter().any(|item| item.tool == "search"));
    }

    #[tokio::test]
    async fn repeated_timeout_aborts_as_tool_failed() {
        let hanging = tools_body(vec![tool_call("c1", "search", r#"{"query": "a"}"#)], "");
        let double = spawn_double(vec![(200, hanging.clone()), (200, hanging)]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![ToolResult::Hang, ToolResult::Hang]),
            &loop_input(&["search"]),
            &LoopBudget {
                tool_timeout: Duration::from_millis(50),
                max_repairs: 1,
                ..LoopBudget::default()
            },
        )
        .await
        .expect_err("timeout streak must abort");
        match err {
            LoopError::ToolFailed { reason } => assert!(reason.contains("timeout after")),
            other => panic!("expected ToolFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn strict_repairs_zero_aborts_on_first_failure() {
        let double = spawn_double(vec![(
            200,
            tools_body(vec![tool_call("c1", "nope", "{}")], ""),
        )]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget {
                max_repairs: 0,
                ..LoopBudget::default()
            },
        )
        .await
        .expect_err("max_repairs 0 must abort immediately");
        assert!(matches!(err, LoopError::InvalidToolCall { .. }));
    }

    #[tokio::test]
    async fn empty_answer_repairs() {
        let double = spawn_double(vec![(200, empty_body()), (200, text_body(&final_answer()))]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect("empty answer must repair");
        assert_eq!(report.turns_used, 2);
        assert_eq!(report.failures_used, 1);
    }

    #[tokio::test]
    async fn budget_exhausted_aborts() {
        let tool_turn = tools_body(vec![tool_call("c1", "search", r#"{"query": "a"}"#)], "");
        let double = spawn_double(vec![(200, tool_turn.clone()), (200, tool_turn)]);
        let err = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![canned_search(), canned_search()]),
            &loop_input(&["search"]),
            &LoopBudget {
                max_turns: 2,
                ..LoopBudget::default()
            },
        )
        .await
        .expect_err("no FINAL within budget must abort");
        match err {
            LoopError::BudgetExhausted { turns } => assert_eq!(turns, 2),
            other => panic!("expected BudgetExhausted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_grows_across_turns() {
        let double = spawn_double(vec![
            (
                200,
                tools_body(vec![tool_call("c1", "search", r#"{"query": "a"}"#)], ""),
            ),
            (
                200,
                tools_body(
                    vec![tool_call("c2", "search", r#"{"query": "b"}"#)],
                    "still looking",
                ),
            ),
            (200, text_body(&final_answer())),
        ]);
        let gateway = gateway_at(&double.base_url);
        let tools = stub_tools(vec![canned_search(), canned_search()]);
        let input = loop_input(&["search"]);
        let budget = LoopBudget::default();
        let roster: Vec<(&str, &str)> = tools.tool_purposes();
        let system_prompt = build_system_prompt(&roster, input.size);
        let defs = tools.tool_defs(&input.allowed_tools);
        let mut history: Vec<crate::shared::agent_loop::context::HistoryEntry> = Vec::new();
        let mut lengths = Vec::new();
        for _ in 1..=3 {
            let messages = crate::shared::agent_loop::context::assemble(
                &system_prompt,
                &input,
                &history,
                &budget,
            );
            lengths.push(messages.len());
            let reply = if defs.is_empty() {
                gateway.chat(&messages).await.expect("double must answer")
            } else {
                gateway
                    .chat_with_tools(&messages, &defs, Some(ToolChoice::Auto))
                    .await
                    .expect("double must answer")
            };
            if reply.tool_calls.is_empty() {
                break;
            }
            history.push(
                crate::shared::agent_loop::context::HistoryEntry::assistant_for_test(reply.output),
            );
            history.push(
                crate::shared::agent_loop::context::HistoryEntry::observation_for_test(
                    "TOOL RESULTS:\n[row]: ok".to_owned(),
                ),
            );
        }
        assert_eq!(lengths.len(), 3);
        assert!(lengths[0] < lengths[1], "context must grow: {lengths:?}");
        assert!(lengths[1] < lengths[2], "context must grow: {lengths:?}");
    }

    #[tokio::test]
    async fn partial_execution_failure_continues_with_next_call() {
        let double = spawn_double(vec![
            (
                200,
                tools_body(
                    vec![
                        tool_call("c1", "search", r#"{"query": "a"}"#),
                        tool_call("c2", "search", r#"{"query": "b"}"#),
                    ],
                    "",
                ),
            ),
            (200, text_body(&final_answer())),
        ]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![
                ToolResult::Failed {
                    tool: "search".to_owned(),
                    reason: "boom".to_owned(),
                },
                canned_search(),
            ]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect("execution failure must not stop sibling calls");
        assert_eq!(report.turns_used, 2);
        assert_eq!(report.evidence.len(), 2);
        assert_eq!(report.failures_used, 1);
        assert_eq!(report.evidence[0].tool, "search");
    }

    #[tokio::test]
    async fn mixed_success_resets_failure_streak() {
        let failing = tools_body(vec![tool_call("c1", "search", "{}")], "");
        let mixed = tools_body(
            vec![
                tool_call("c1", "search", r#"{"query": "a"}"#),
                tool_call("c2", "search", r#"{"query": "b"}"#),
            ],
            "",
        );
        let double = spawn_double(vec![
            (200, failing),
            (200, mixed),
            (200, text_body(&final_answer())),
        ]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![
                ToolResult::Failed {
                    tool: "search".to_owned(),
                    reason: "boom".to_owned(),
                },
                ToolResult::Failed {
                    tool: "search".to_owned(),
                    reason: "boom".to_owned(),
                },
                canned_search(),
            ]),
            &loop_input(&["search"]),
            &LoopBudget {
                max_repairs: 1,
                ..LoopBudget::default()
            },
        )
        .await
        .expect("a later success must reset the consecutive counter");
        assert_eq!(report.turns_used, 3);
    }

    #[tokio::test]
    async fn unknown_tool_after_success_still_repairs() {
        let double = spawn_double(vec![
            (
                200,
                tools_body(
                    vec![tool_call(
                        "c1",
                        "fetch",
                        r#"{"url": "https://example.com/a"}"#,
                    )],
                    "",
                ),
            ),
            (200, text_body(&final_answer())),
        ]);
        let report = run_loop(
            &gateway_at(&double.base_url),
            &stub_tools(vec![canned_fetch()]),
            &loop_input(&["search"]),
            &LoopBudget::default(),
        )
        .await
        .expect("call outside allowed_tools must repair, not execute");
        assert_eq!(report.turns_used, 2);
        assert_eq!(report.failures_used, 1);
        assert!(report.evidence.is_empty());
    }

    #[test]
    fn loop_error_display_shapes() {
        assert!(LoopError::NoStart("x".to_owned())
            .to_string()
            .contains("cannot start"));
        assert!(LoopError::BudgetExhausted { turns: 2 }
            .to_string()
            .contains('2'));
        let from_gateway: LoopError = GatewayError::Auth.into();
        assert!(matches!(
            from_gateway,
            LoopError::Gateway(GatewayError::Auth)
        ));
    }
}
