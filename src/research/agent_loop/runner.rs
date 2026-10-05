//! Agent loop runner with turn budget, run deadline and stop condition.
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
//! The turn budget starts at `max_turns` and grows by
//! [`SEARCH_TURN_BONUS`](crate::research::agent_loop::types::SEARCH_TURN_BONUS)
//! for each successful `search` call after the first, up to
//! `max_turns_cap` (#101). The final turn (last of the current budget, or
//! the first turn once the run deadline's final reserve is reached, #100)
//! sends the same tool defs with `ToolChoice::None`; tool calls in its
//! reply are not dispatched and count as an empty-answer repair.
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
    if budget.max_turns_cap < budget.max_turns {
        return Err(LoopError::NoStart(format!(
            "max_turns_cap ({}) must be >= max_turns ({})",
            budget.max_turns_cap, budget.max_turns
        )));
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
    let mut successful_searches: u32 = 0;
    let mut turn: u32 = 0;

    loop {
        let turn_budget = budget.turn_budget(successful_searches);
        if budget.deadline_passed() {
            return Err(LoopError::DeadlineExceeded { turns: turn });
        }
        turn += 1;
        let deadline_final = budget.in_final_reserve();
        let final_turn = turn >= turn_budget || deadline_final;
        let messages = context::assemble(&system_prompt, input, &history, budget);
        let reply = if defs.is_empty() {
            gateway.chat(&messages).await
        } else {
            // Same defs on the final turn, so the request prefix stays
            // cacheable; only the choice changes.
            let choice = if final_turn {
                ToolChoice::None
            } else {
                ToolChoice::Auto
            };
            gateway
                .chat_with_tools(&messages, &defs, Some(choice))
                .await
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(GatewayError::MissingApiKey) if turn == 1 => {
                return Err(LoopError::NoStart("GATEWAY_API_KEY is not set".to_owned()));
            }
            Err(GatewayError::Timeout) if budget.deadline_passed() => {
                return Err(LoopError::DeadlineExceeded { turns: turn });
            }
            Err(err) => return Err(LoopError::Gateway(err)),
        };
        usage.add(&reply.usage);
        if reply.finish_reason.as_deref() == Some("length") {
            tracing::warn!("agent loop turn {turn} hit the model length limit; continuing");
        }

        // Turns left after this one, as the model is told: none past the
        // final turn, one once the deadline's final reserve is reached.
        let turns_left = |turn_budget: u32| {
            if budget.in_final_reserve() {
                1
            } else {
                turn_budget.saturating_sub(turn)
            }
        };
        let end_without_answer = || {
            if deadline_final {
                LoopError::DeadlineExceeded { turns: turn }
            } else {
                LoopError::BudgetExhausted { turns: turn }
            }
        };

        if reply.tool_calls.is_empty() || final_turn {
            let answer = if reply.tool_calls.is_empty() {
                parse_answer(&reply.output, input.size).ok()
            } else {
                tracing::warn!(
                    "agent loop turn {turn} called tools on the final turn; not dispatched"
                );
                None
            };
            if let Some(mut synthesis) = answer {
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
                    turn_budget,
                    failures_used,
                    evidence,
                    usage,
                    search_blocked,
                    had_search_hits,
                });
            }
            consecutive_failures += 1;
            failures_used += 1;
            if consecutive_failures > budget.max_repairs {
                return Err(LoopError::InvalidToolCall {
                    reason: "empty answer".to_owned(),
                });
            }
            if final_turn {
                return Err(end_without_answer());
            }
            let observation =
                empty_answer_observation(tools, &input.allowed_tools, turns_left(turn_budget));
            history.push(HistoryEntry::TextTurn {
                assistant: reply.output.clone(),
                observation,
            });
            continue;
        }

        let mut outcome = dispatch_turn(
            turn,
            &reply.tool_calls,
            tools,
            &input.allowed_tools,
            budget,
            budget.work_cutoff(),
            &nonce,
        )
        .await;
        evidence.extend(outcome.evidence);
        if outcome.search_blocked.is_some() {
            search_blocked = outcome.search_blocked;
        }
        had_search_hits = had_search_hits || outcome.had_search_hits;
        successful_searches = successful_searches.saturating_add(outcome.successful_searches);
        if let Some(newest) = outcome.results.last_mut() {
            let left = turns_left(budget.turn_budget(successful_searches));
            newest.content.push_str(&turns_left_note(left));
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
    }
}

#[cfg(test)]
mod tests {
    mod basic;
    mod budget_and_mix;
    mod helpers;
    mod no_start;
    mod repairs;
    mod search_blocked;
    mod turn_budget;
    mod wire_shape;
}
