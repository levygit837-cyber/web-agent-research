//! One Research end to end: run the agent loop, synthesize, persist.
//!
//! Deep module behind the public interface (ADR-0006): `run_research()` owns
//! gateway construction, `run_loop` wiring, JSONL append, and the response
//! shape. `src/main.rs` is the only adapter: it prints `Ok` to stdout
//! (`--json` or the human `render`) and maps `Err` to a CLI exit code via
//! `exit_for`. The wire DTOs (`ResearchRequest`, `ResearchResponse`, ...)
//! are in `dto.rs`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::llm::{Gateway, GatewayConfig};
use crate::research::agent_loop::{run_loop, LoopBudget, LoopError, LoopInput, ToolRegistry};
use crate::research::dto::{ResearchRequest, ResearchResponse, SynthesisDTO, UsageDTO};
use crate::research::session::{append_header, append_turn, SessionHeader, SessionUsage, TurnRow};
use crate::web::fetch::{Evidence, Fetcher};
use crate::web::search::tool::Searcher;

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
    // Sized before `Gateway::new` consumes the config. `--size` does not
    // scale it: the window is a property of the model, not of the answer.
    let budget = LoopBudget::for_window(
        req.max_turns,
        config.context_window_tokens,
        config.reply_reserve_tokens(),
    )
    .map_err(ResearchError::NotConfigured)?;
    let gateway = Gateway::new(config);
    let input = LoopInput {
        goal: req.goal.clone(),
        size: req.size,
        allowed_tools: vec!["search".to_owned(), "fetch".to_owned()],
    };
    let report = run_loop(&gateway, &tools, &input, &budget)
        .await
        .map_err(map_loop_error)?;

    let session_path = req
        .session_out
        .clone()
        .unwrap_or_else(|| PathBuf::from("sessions").join(format!("{session_id}.jsonl")));
    let synthesis_dto = SynthesisDTO::from(report.synthesis);
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
        // The Session is the record: it holds the whole page, once per URL,
        // from the call that fetched it. Continuation parts and repeats
        // carry no `page` and add no row (#80).
        let evidence: Vec<Evidence> = report
            .evidence
            .iter()
            .filter(|item| item.turn == turn)
            .filter_map(|item| item.page.as_deref().cloned())
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
    mod gateway_integration;
    mod helpers;
    mod validation;
}
