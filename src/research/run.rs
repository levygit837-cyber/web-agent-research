//! One Research end to end: run the agent loop, synthesize, persist.
//!
//! Deep module behind the public interface (ADR-0006): `run_research()` owns
//! gateway construction, `run_loop` wiring, JSONL append, and the response
//! shape. `src/main.rs` is the only adapter: it prints `Ok` to stdout
//! (`--json` or the human `render`) and maps `Err` to a CLI exit code via
//! `ResearchError::exit_code`. The wire DTOs (`ResearchRequest`, `ResearchResponse`, ...)
//! are in `dto.rs`.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::llm::{Gateway, GatewayConfig};
use crate::research::agent_loop::tape::Tape;
use crate::research::agent_loop::{
    run_loop_with_events, LoopBudget, LoopError, LoopInput, ToolRegistry,
};
use crate::research::data_dir;
use crate::research::dto::{
    ResearchRequest, ResearchResponse, SynthesisDTO, UsageDTO, CONTENT_TRUST_UNTRUSTED_WEB,
};
use crate::research::events::Events;
use crate::research::session::{SessionHeader, SessionSink, SessionUsage};
use crate::research::synthesis::Support;
use crate::web::fetch::Fetcher;
use crate::web::search::tool::Searcher;

/// Failures of one research run; each maps to a CLI exit code via
/// [`ResearchError::exit_code`] (`NotConfigured→2`, `GatewayExhausted→3`,
/// `ToolFailure→4`, `BudgetExhausted→5`, `Io→6`, `SearchBlocked→7`,
/// `GatewayRejected→8`, `DeadlineExceeded→9`, `Interrupted→130`).
#[derive(Debug)]
pub enum ResearchError {
    /// Empty goal, `max_turns == 0`, `max_turns_cap < max_turns`,
    /// `deadline_secs == 0`, invalid `--session-id`, malformed
    /// `GATEWAY_BASE_URL`, missing `GATEWAY_API_KEY` — no I/O attempted.
    NotConfigured(String),
    /// Gateway retries exhausted on a retryable failure (429, 5xx,
    /// timeout, transport), incl. the 429-credit wall.
    GatewayExhausted(String),
    /// The gateway rejected the request deterministically (401, 403, other
    /// 4xx, an error object in a 2xx body, unparsable 2xx, empty choices,
    /// refusal): retrying cannot help, the configuration must change.
    GatewayRejected(String),
    /// Invalid tool call or tool failure after `max_repairs`.
    ToolFailure(String),
    /// The turn budget was consumed with no FINAL parsed.
    BudgetExhausted { turns: u32 },
    /// The run deadline (`deadline_secs`) passed with no usable answer.
    DeadlineExceeded { turns: u32 },
    /// Explicit `--session-out` write failure. A defaulted Session path never
    /// raises this: it warns on stderr and the run still succeeds (#102).
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
    /// `ctrl_c` ended the Session (#150). Only the CLI entry point raises it;
    /// the Session file ends with `session.finished` `outcome: interrupted`.
    Interrupted,
}

impl std::fmt::Display for ResearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured(reason) => write!(f, "research not configured: {reason}"),
            Self::GatewayExhausted(reason) => write!(f, "gateway exhausted: {reason}"),
            Self::GatewayRejected(reason) => write!(f, "gateway rejected the request: {reason}"),
            Self::ToolFailure(reason) => write!(f, "tool failure: {reason}"),
            Self::BudgetExhausted { turns } => {
                write!(f, "turn budget exhausted after {turns} turns")
            }
            Self::DeadlineExceeded { turns } => {
                write!(
                    f,
                    "run deadline exceeded after {turns} turns with no answer"
                )
            }
            Self::Io(reason) => write!(f, "session write failed: {reason}"),
            Self::SearchBlocked(detail) => {
                write!(f, "search blocked by a bot-detection challenge: {detail}")
            }
            Self::Interrupted => write!(f, "interrupted"),
        }
    }
}

impl std::error::Error for ResearchError {}

impl ResearchError {
    /// The CLI exit code, and the only place the mapping is written
    /// (`docs/harness.md` "Exit codes"): `main.rs` exits with it and
    /// `session.finished` carries it.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::NotConfigured(_) => 2,
            Self::GatewayExhausted(_) => 3,
            Self::ToolFailure(_) => 4,
            Self::BudgetExhausted { .. } => 5,
            Self::Io(_) => 6,
            Self::SearchBlocked(_) => 7,
            Self::GatewayRejected(_) => 8,
            Self::DeadlineExceeded { .. } => 9,
            Self::Interrupted => 130,
        }
    }

    /// The `outcome` of `session.finished` for this failure (ADR-0010 §3).
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::NotConfigured(_) => "not_configured",
            Self::GatewayExhausted(_) => "gateway_exhausted",
            Self::ToolFailure(_) => "tool_failure",
            Self::BudgetExhausted { .. } => "budget_exhausted",
            Self::Io(_) => "session_io",
            Self::SearchBlocked(_) => "search_blocked",
            Self::GatewayRejected(_) => "gateway_rejected",
            Self::DeadlineExceeded { .. } => "deadline_exceeded",
            Self::Interrupted => "interrupted",
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
            } else if gateway_err.is_retryable() {
                ResearchError::GatewayExhausted(message)
            } else {
                ResearchError::GatewayRejected(message)
            }
        }
        LoopError::InvalidToolCall { reason } | LoopError::ToolFailed { reason } => {
            ResearchError::ToolFailure(reason)
        }
        LoopError::BudgetExhausted { turns } => ResearchError::BudgetExhausted { turns },
        LoopError::DeadlineExceeded { turns } => ResearchError::DeadlineExceeded { turns },
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
/// The eval-only `WAR_EVAL_RECORD` / `WAR_EVAL_REPLAY` /
/// `WAR_EVAL_REPLAY_THROUGH` env vars (#107, #129, `docs/eval.md`) record,
/// replay, or replay-and-extend the tools; a bad value, or more than one
/// set, is `NotConfigured`. `ctrl_c` ends the Session with
/// [`ResearchError::Interrupted`] (#150).
pub async fn run_research(req: ResearchRequest) -> Result<ResearchResponse, ResearchError> {
    let tape = Tape::from_env().map_err(ResearchError::NotConfigured)?;
    let tools = ToolRegistry::new(Searcher::new(), Fetcher::new()).with_tape(tape);
    run_research_until(req, tools, ctrl_c()).await
}

/// Resolves when the process receives `ctrl_c`. If the handler cannot be
/// installed it never resolves: a Session must not report an interrupt that
/// did not happen.
async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// [`run_research`] over an injected registry (test seam for hermetic runs),
/// with no interrupt.
#[cfg(test)]
pub(crate) async fn run_research_with(
    req: ResearchRequest,
    tools: ToolRegistry,
) -> Result<ResearchResponse, ResearchError> {
    run_research_until(req, tools, std::future::pending()).await
}

/// The Session behind [`run_research`]: `interrupt` resolving ends it as
/// [`ResearchError::Interrupted`]. The race sits here, around the whole
/// Session body, so `session.finished` is still emitted once, from the one
/// place every exit passes through.
pub(crate) async fn run_research_until(
    req: ResearchRequest,
    tools: ToolRegistry,
    interrupt: impl Future<Output = ()>,
) -> Result<ResearchResponse, ResearchError> {
    // The Session id is the first thing resolved: Events name their Session,
    // so an invalid `--session-id` exits 2 with no Events (ADR-0010 §3).
    if let Some(id) = req.session_id.as_deref() {
        data_dir::validate_session_id(id).map_err(ResearchError::NotConfigured)?;
    }
    // Generated before the loop (not after) so it can be sent as
    // `prompt_cache_key` on every turn of this run, keeping cache-routing
    // stable across the whole Session rather than only the persisted rows.
    let session_id = req.session_id.clone().unwrap_or_else(generate_session_id);
    // An `--events` output that cannot be opened is a configuration error,
    // raised before any gateway call.
    let events = Events::open(
        &session_id,
        req.events.as_ref(),
        req.events_detail,
        req.progress,
    )
    .map_err(ResearchError::NotConfigured)?;
    if req.trace {
        events.enable_trace();
    }
    let session = run_session(&req, session_id, tools, &events);
    // Work first: an answer that is ready wins over a late interrupt.
    let result = tokio::select! {
        biased;
        result = session => result,
        () = interrupt => Err(ResearchError::Interrupted),
    };
    // A mid-Session write failure on an explicit path surfaces now, after
    // the Session finished (a Session that already failed keeps its error).
    let result = match (result, events.session_log_failure()) {
        (Ok(_), Some(reason)) => Err(ResearchError::Io(reason)),
        (result, _) => result,
    };
    // Every exit of the Session body, present or future, passes through this
    // one emit: `session.finished` is the last Event of the Session.
    events.session_finished(&result);
    events.finish_session_log();
    result
}

/// The Session body behind [`run_research_until`]: validation and the loop.
/// Returns early freely; the caller emits `session.finished`.
async fn run_session(
    req: &ResearchRequest,
    session_id: String,
    tools: ToolRegistry,
    events: &Events,
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
    if req.max_turns_cap < req.max_turns {
        return Err(ResearchError::NotConfigured(format!(
            "max_turns_cap ({}) must be >= max_turns ({})",
            req.max_turns_cap, req.max_turns
        )));
    }
    if req.deadline_secs < 1 {
        return Err(ResearchError::NotConfigured(
            "deadline_secs must be >= 1".to_owned(),
        ));
    }
    let config = GatewayConfig::from_env()
        .map_err(|err| ResearchError::NotConfigured(err.to_string()))?
        .with_prompt_cache_key(session_id.clone());
    // Sized before `Gateway::new` consumes the config. `--size` does not
    // scale it: the window is a property of the model, not of the answer.
    let run_time = Duration::from_secs(req.deadline_secs);
    let budget = LoopBudget {
        max_turns_cap: req.max_turns_cap,
        ..LoopBudget::for_window(
            req.max_turns,
            config.context_window_tokens,
            config.reply_reserve_tokens(),
        )
        .map_err(ResearchError::NotConfigured)?
    }
    .with_deadline(run_time, config.request_timeout);
    let deadline = budget
        .deadline
        .expect("with_deadline always sets the run deadline");
    // Configuration is valid: the Session starts, and its file with it
    // (#150). The header is line 0, so it goes before `session.started`; a
    // configuration failure above leaves no file and an unwritable explicit
    // `--session-out` is exit 6 here, before any gateway call.
    let header = SessionHeader::new(
        session_id.clone(),
        req.goal.clone(),
        req.size.as_str().to_owned(),
    );
    events
        .attach_session_log(
            SessionSink::new(req.session_out.clone(), &session_id),
            &header,
        )
        .map_err(ResearchError::Io)?;
    events.session_started(req, &config);
    let gateway = Gateway::new(config).with_deadline(deadline);
    let input = LoopInput {
        goal: req.goal.clone(),
        size: req.size,
        allowed_tools: vec!["search".to_owned(), "fetch".to_owned()],
    };
    let report = run_loop_with_events(&gateway, &tools, &input, &budget, events)
        .await
        .map_err(map_loop_error)?;

    let synthesis_dto = SynthesisDTO::from(report.synthesis);
    let usage_dto = UsageDTO::from(report.usage);
    // The final Turn's row: the loop wrote the others as their Turns ended.
    // It carries the Synthesis and run usage as in format 1, also when the
    // Session then fails with exit 7 (the audit trail keeps what the model
    // answered; `session.finished` says it was not delivered).
    let synthesis_value =
        serde_json::to_value(&synthesis_dto).map_err(|err| ResearchError::Io(err.to_string()))?;
    events.turn_ended(
        report.turns_used,
        &report.evidence,
        Some((&synthesis_value, SessionUsage::from(usage_dto.clone()))),
    );
    // Continuation parts (#80) each carry the page URL; the response lists
    // every fetched source once, in first-fetched order.
    let mut evidence_urls: Vec<String> = Vec::new();
    for url in report.evidence.iter().filter_map(|item| item.url.as_ref()) {
        if !evidence_urls.contains(url) {
            evidence_urls.push(url.clone());
        }
    }
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

    Ok(ResearchResponse {
        session_id,
        content_trust: CONTENT_TRUST_UNTRUSTED_WEB.to_owned(),
        synthesis: synthesis_dto,
        turns_used: report.turns_used,
        turn_budget: report.turn_budget,
        evidence_urls,
        usage: usage_dto,
    })
}

/// Pure printer for human stdout: summary, `##` theme sections with `-`
/// points, then a numbered `Sources:` `[title](url)` list in document order.
/// A source whose page did not hold the quote of any bullet citing it is
/// marked `(unsupported: …)`, one that held the quote but not every number
/// or identifier `(partial: …)` (#106).
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
        let mark = match citation.support {
            Support::None => " (unsupported: the cited page does not contain the quoted text)",
            Support::Partial => {
                " (partial: a number or identifier in the claim is not on the cited page)"
            }
            Support::Exact | Support::Unchecked => "",
        };
        out.push_str(&format!(
            "{}. [{}]({}){mark}\n",
            index + 1,
            title,
            citation.url
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    mod events;
    mod gateway_integration;
    mod helpers;
    mod model_stream;
    mod session_log;
    mod session_path;
    mod tool_events;
    mod trace;
    mod validation;
}
