//! One turn of tool dispatch and the per-turn prompt notes.
//!
//! [`dispatch_turn`] executes a turn's calls through the registry and pairs
//! every call with exactly one `tool` message; the note helpers build the
//! text the runner appends to the transcript tail (turns left, the tool
//! roster after an empty reply). `runner.rs` owns the loop around them.

use tokio::time::Instant;

use crate::llm::RequestedToolCall;
use crate::research::agent_loop::answer::{ANSWER_CLOSE, ANSWER_OPEN};
use crate::research::agent_loop::context::{self, ToolMessage};
use crate::research::agent_loop::registry::ToolRegistry;
use crate::research::agent_loop::result::{FailureKind, ToolResult};
use crate::research::agent_loop::types::{LoopBudget, ToolEvidence};

pub(super) fn allowed_roster(
    tools: &ToolRegistry,
    allowed: &[String],
) -> Vec<(&'static str, &'static str)> {
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

/// Repair observation after a reply with no tool call and no answer text.
/// It keeps the turns-left countdown (#72); `turns_left` counts the turns
/// after this one. When only the final turn is left it asks for the answer
/// alone, since the final turn runs tools-off.
pub(super) fn empty_answer_observation(
    tools: &ToolRegistry,
    allowed: &[String],
    turns_left: u32,
) -> String {
    let note = turns_left_note(turns_left);
    if turns_left <= 1 {
        return format!("Your last reply had no tool call and no answer text.{note}");
    }
    format!(
        "Your last reply had no tool call and no answer text. Continue the research with a tool call, or reply with the final answer inside {ANSWER_OPEN}{ANSWER_CLOSE} tags.\nAvailable tools:\n{}{note}",
        roster_footer(tools, allowed)
    )
}

/// Turn-budget note appended to the newest tool result of each tool turn
/// (#72). The system prompt must stay byte-identical across turns for
/// prompt caching, so per-turn state rides in the transcript tail; the note
/// is stored in history, so the transcript stays append-only. `turns_left`
/// counts the turns after this one under the current (possibly extended)
/// budget.
pub(super) fn turns_left_note(turns_left: u32) -> String {
    match turns_left {
        0 => String::new(),
        1 => format!(
            "\n\n[1 turn left: tools are off on it, so your next reply must be the final answer inside {ANSWER_OPEN}{ANSWER_CLOSE} tags.]"
        ),
        left => format!("\n\n[{left} turns left, counting the final answer.]"),
    }
}

fn cap_excerpt(rendered: &str, budget: &LoopBudget) -> String {
    context::cap_evidence(rendered, budget.max_part_chars)
}

/// Stable synthetic id for a call the model sent without one. Turn-scoped
/// and 1-based so ids stay human-legible across a run's tool_calls/tool
/// pairs (`call_<turn>_<index>`).
fn synthesize_id(turn: u32, index: usize) -> String {
    format!("call_{turn}_{}", index + 1)
}

/// Resolve every call's wire id up front so the assistant `tool_calls`
/// message and each call's `tool` message agree, even when the model sent
/// no id at all.
fn resolve_call_ids(turn: u32, calls: &[RequestedToolCall]) -> Vec<RequestedToolCall> {
    calls
        .iter()
        .enumerate()
        .map(|(index, call)| RequestedToolCall {
            id: if call.id.is_empty() {
                synthesize_id(turn, index)
            } else {
                call.id.clone()
            },
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        })
        .collect()
}

pub(super) struct TurnOutcome {
    /// Every requested call, ids resolved, in wire order — the assistant
    /// `tool_calls` message for this turn.
    pub(super) calls: Vec<RequestedToolCall>,
    /// One `tool` message per call above, same order: every id gets exactly
    /// one paired result, whether executed, failed, or dropped over cap.
    pub(super) results: Vec<ToolMessage>,
    pub(super) evidence: Vec<ToolEvidence>,
    pub(super) had_success: bool,
    pub(super) failure: Option<FailureKind>,
    pub(super) failure_reason: String,
    /// Detail of the last `search` call this turn that came back fully
    /// Challenge-walled (#53); `None` if none did.
    pub(super) search_blocked: Option<String>,
    /// Whether any `search` call this turn returned a non-empty Hit list.
    pub(super) had_search_hits: bool,
    /// `search` calls this turn that succeeded; each one after the run's
    /// first extends the turn budget (#101).
    pub(super) successful_searches: u32,
}

/// Execute one turn's calls (up to `max_tools_per_turn`) and pair every one
/// — executed, failed, timed out, disallowed, or dropped over cap — with a
/// `tool` message. Execution failures never stop sibling calls; the last
/// failure in the turn wins for `failure`/`failure_reason` (a turn mixing
/// failure kinds is exceptional and untested — either kind aborts the run
/// the same way once repairs are exhausted). `cutoff` is the latest instant
/// a call may run to (the run deadline minus the final-answer reserve,
/// #100); each call's timeout is `tool_timeout` clamped to it. `nonce` tags
/// the containers that frame page and Hit text (#98).
pub(super) async fn dispatch_turn(
    turn: u32,
    calls: &[RequestedToolCall],
    tools: &ToolRegistry,
    allowed: &[String],
    budget: &LoopBudget,
    cutoff: Option<Instant>,
    nonce: &str,
) -> TurnOutcome {
    let cap = budget.max_tools_per_turn.max(1);
    let resolved = resolve_call_ids(turn, calls);

    let mut results: Vec<ToolMessage> = Vec::with_capacity(resolved.len());
    let mut evidence: Vec<ToolEvidence> = Vec::new();
    let mut had_success = false;
    let mut failure: Option<FailureKind> = None;
    let mut failure_reason = String::new();
    let mut search_blocked: Option<String> = None;
    let mut had_search_hits = false;
    let mut successful_searches: u32 = 0;

    for (index, call) in resolved.iter().enumerate() {
        if index >= cap {
            results.push(ToolMessage {
                id: call.id.clone(),
                content: cap_excerpt(
                    &format!("not executed: per-turn tool-call cap ({cap}) reached"),
                    budget,
                ),
            });
            continue;
        }
        let is_allowed = allowed.iter().any(|name| name == &call.name);
        if !is_allowed {
            let reason = format!(
                "unknown tool '{}' (available: {})",
                call.name,
                allowed.join(", ")
            );
            results.push(ToolMessage {
                id: call.id.clone(),
                content: cap_excerpt(&format!("FAILED: {reason}"), budget),
            });
            failure = Some(FailureKind::Dispatch);
            failure_reason = reason;
            continue;
        }
        let timeout = cutoff.map_or(budget.tool_timeout, |cutoff| {
            budget
                .tool_timeout
                .min(cutoff.saturating_duration_since(Instant::now()))
        });
        let executed =
            tokio::time::timeout(timeout, tools.execute(call, budget.max_part_chars)).await;
        match executed {
            Err(_) => {
                let reason = if timeout < budget.tool_timeout {
                    format!(
                        "timeout after {}s: the run deadline is near",
                        timeout.as_secs()
                    )
                } else {
                    format!("timeout after {}s", budget.tool_timeout.as_secs())
                };
                // Same text as an executor failure, so a timed-out fetch
                // carries the failed-fetch next step (#72).
                let rendered = ToolResult::Failed {
                    tool: call.name.clone(),
                    reason: reason.clone(),
                    kind: FailureKind::Execution,
                }
                .render(nonce);
                let content = cap_excerpt(&rendered, budget);
                results.push(ToolMessage {
                    id: call.id.clone(),
                    content: content.clone(),
                });
                evidence.push(ToolEvidence {
                    turn,
                    id: call.id.clone(),
                    tool: call.name.clone(),
                    excerpt: content,
                    url: None,
                    fetch_path: None,
                    page: None,
                });
                failure = Some(FailureKind::Execution);
                failure_reason = reason;
            }
            Ok(result) => {
                let rendered = result.render(nonce);
                // A Fetch part is already sized by the registry; capping it
                // again would cut the part's continuation line.
                let excerpt = if matches!(result, ToolResult::Fetch { .. }) {
                    rendered.clone()
                } else {
                    cap_excerpt(&rendered, budget)
                };
                results.push(ToolMessage {
                    id: call.id.clone(),
                    content: excerpt.clone(),
                });
                if let ToolResult::Search { hits, .. } = &result {
                    if !hits.is_empty() {
                        had_search_hits = true;
                    }
                }
                if let ToolResult::SearchBlocked { detail } = &result {
                    search_blocked = Some(detail.clone());
                }
                if result.is_success() {
                    had_success = true;
                    if matches!(result, ToolResult::Search { .. }) {
                        successful_searches += 1;
                    }
                    evidence.push(ToolEvidence {
                        turn,
                        id: call.id.clone(),
                        tool: call.name.clone(),
                        excerpt,
                        url: result.url(),
                        fetch_path: result.fetch_path(),
                        page: result.first_delivery_page(),
                    });
                } else {
                    let kind = result
                        .failure_kind()
                        .expect("non-success ToolResult always carries a FailureKind");
                    let reason = match &result {
                        ToolResult::Failed { reason, .. } => reason.clone(),
                        _ => rendered.clone(),
                    };
                    evidence.push(ToolEvidence {
                        turn,
                        id: call.id.clone(),
                        tool: call.name.clone(),
                        excerpt,
                        url: None,
                        fetch_path: None,
                        page: None,
                    });
                    failure = Some(kind);
                    failure_reason = reason;
                }
            }
        }
    }

    TurnOutcome {
        calls: resolved,
        results,
        evidence,
        had_success,
        failure,
        failure_reason,
        search_blocked,
        had_search_hits,
        successful_searches,
    }
}
