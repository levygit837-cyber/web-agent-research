//! Agent loop runner with turn budget and stop condition.
//!
//! One function is the external seam: [`run_loop`] borrows the gateway and
//! the tool registry, offers `allowed_tools` with `ToolChoice::Auto` (plain
//! `chat()` for zero-tool runs), feeds tool results back as native
//! `role: "tool"` messages paired to the model's own `tool_calls`, and
//! repeats until the first tool-free non-empty answer, which becomes the
//! typed [`Synthesis`](crate::research::synthesis::Synthesis). Gateway
//! errors reaching the loop are final for that turn (the gateway owns
//! retries); tool and dispatch failures are model-visible tool-role
//! results bounded by `max_repairs`.
//!
//! The contract types are in `types.rs`; per-turn dispatch and the prompt
//! notes are in `dispatch.rs`.

use crate::llm::{Gateway, GatewayError, TokenUsage, ToolChoice};
use crate::research::agent_loop::answer::{parse_answer, retain_fetched_citations};
use crate::research::agent_loop::context::{self, HistoryEntry};
use crate::research::agent_loop::dispatch::{
    allowed_roster, dispatch_turn, empty_answer_observation, turns_left_note,
};
use crate::research::agent_loop::registry::ToolRegistry;
use crate::research::agent_loop::result::{new_nonce, FailureKind};
use crate::research::agent_loop::types::{
    LoopBudget, LoopError, LoopInput, RunReport, ToolEvidence,
};
use crate::research::agent_loop::verify::verify_synthesis;
use crate::research::prompt::build_system_prompt;
use crate::web::search::dedup_key;

/// Run the multi-turn research loop until the first tool-free answer.
///
/// Borrows the gateway and registry; exactly one gateway call per turn.
/// See the module docs above for the full contract.
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
    let system_prompt = build_system_prompt(&roster, input.size, budget);
    let defs = tools.tool_defs(&input.allowed_tools);
    // One nonce per run tags the containers around page and Hit text (#98):
    // a page cannot predict it, so it cannot forge a closing tag.
    let nonce = new_nonce();

    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut evidence: Vec<ToolEvidence> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut consecutive_failures: u32 = 0;
    let mut failures_used: u32 = 0;
    let mut search_blocked: Option<String> = None;
    let mut had_search_hits = false;

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
        usage.add(&reply.usage);
        if reply.finish_reason.as_deref() == Some("length") {
            tracing::warn!("agent loop turn {turn} hit the model length limit; continuing");
        }

        if reply.tool_calls.is_empty() {
            match parse_answer(&reply.output, input.size) {
                Ok(mut synthesis) => {
                    let fetched: std::collections::HashSet<String> = evidence
                        .iter()
                        .filter_map(|item| item.url.as_deref())
                        .map(dedup_key)
                        .collect();
                    retain_fetched_citations(&mut synthesis, &fetched);
                    verify_synthesis(&mut synthesis, &evidence);
                    return Ok(RunReport {
                        synthesis,
                        turns_used: turn,
                        failures_used,
                        evidence,
                        usage,
                        search_blocked,
                        had_search_hits,
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
                    let observation =
                        empty_answer_observation(tools, &input.allowed_tools, turn, budget);
                    history.push(HistoryEntry::TextTurn {
                        assistant: reply.output.clone(),
                        observation,
                    });
                    if turn == budget.max_turns {
                        return Err(LoopError::BudgetExhausted { turns: turn });
                    }
                    continue;
                }
            }
        }

        let mut outcome = dispatch_turn(
            turn,
            &reply.tool_calls,
            tools,
            &input.allowed_tools,
            budget,
            &nonce,
        )
        .await;
        evidence.extend(outcome.evidence);
        if outcome.search_blocked.is_some() {
            search_blocked = outcome.search_blocked;
        }
        had_search_hits = had_search_hits || outcome.had_search_hits;
        if let Some(newest) = outcome.results.last_mut() {
            newest.content.push_str(&turns_left_note(turn, budget));
        }
        history.push(HistoryEntry::ToolTurn {
            text: reply.output.clone(),
            calls: outcome.calls,
            results: outcome.results,
            replay: reply.replay.clone(),
        });
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
    mod basic;
    mod budget_and_mix;
    mod helpers;
    mod no_start;
    mod repairs;
    mod search_blocked;
    mod wire_shape;
}
