//! Contract types of the agent loop: what goes into [`run_loop`] and what
//! comes back.
//!
//! [`LoopInput`] and [`LoopBudget`] configure a run, [`RunReport`] with its
//! [`ToolEvidence`] rows is the success value, [`LoopError`] the failure.
//! Behavior lives in `runner.rs` and `dispatch.rs`; this file is data plus
//! the budget arithmetic.
//!
//! [`run_loop`]: crate::research::agent_loop::run_loop

use std::sync::Arc;
use std::time::Duration;

use crate::llm::{GatewayError, TokenUsage};
use crate::research::synthesis::{Synthesis, SynthesisSize};
use crate::web::fetch::{Evidence, FetchPath};

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

/// Characters per token assumed when sizing the context from a token
/// window. 3 is deliberately conservative: prose averages about 4, but
/// markdown full of URLs, code and identifiers tokenizes worse, and
/// overshooting the window fails the whole run while undershooting only
/// drops old turns sooner.
pub const CHARS_PER_TOKEN: u32 = 3;

/// Ceiling on `max_context_chars` whatever the window. The transcript is
/// resent every turn, so cost and latency grow with it; past this point a
/// bigger window buys less than it costs (#80).
pub const MAX_CONTEXT_CHARS_CEILING: usize = 400_000;

/// Ceiling on `max_part_chars`: one `fetch` part, and the cap on every
/// other tool result.
pub const MAX_PART_CHARS: usize = 24_000;

/// A single part never takes more than `1 / PARTS_PER_CONTEXT` of the
/// context, so a few pages fit before old turns are dropped.
const PARTS_PER_CONTEXT: usize = 8;

/// Smallest context a run can work in: the system prompt and tool
/// definitions take about 6,000 characters, and a part under 2,000 (an
/// eighth of this) splits ordinary pages into dozens of turns. A window
/// that leaves less than this once the reply is reserved is a
/// misconfiguration, not a budget.
pub const MIN_CONTEXT_CHARS: usize = 16_000;

#[derive(Debug, Clone)]
pub struct LoopBudget {
    /// Total gateway calls allowed.
    pub max_turns: u32,
    /// Consecutive failure turns before abort.
    pub max_repairs: u32,
    /// Per-tool-call deadline, applied by the runner.
    pub tool_timeout: Duration,
    /// Size of one `fetch` part, and the cap (with truncation marker) on
    /// every other tool result.
    pub max_part_chars: usize,
    /// Total assembled-char cap before oldest-turn dropping.
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
            max_part_chars: MAX_PART_CHARS,
            max_context_chars: 24_000,
            max_tools_per_turn: 3,
        }
    }
}

impl LoopBudget {
    /// Budget sized to a model window: the window minus the tokens reserved
    /// for the reply, at [`CHARS_PER_TOKEN`], capped at
    /// [`MAX_CONTEXT_CHARS_CEILING`]; a part is at most [`MAX_PART_CHARS`]
    /// and at most an eighth of the context. Errs when the window leaves
    /// less than [`MIN_CONTEXT_CHARS`], so `max_part_chars` is never tiny.
    pub fn for_window(
        max_turns: u32,
        window_tokens: u32,
        reply_reserve_tokens: u32,
    ) -> Result<Self, String> {
        let usable_tokens = window_tokens.saturating_sub(reply_reserve_tokens) as usize;
        let max_context_chars = usable_tokens
            .saturating_mul(CHARS_PER_TOKEN as usize)
            .min(MAX_CONTEXT_CHARS_CEILING);
        if max_context_chars < MIN_CONTEXT_CHARS {
            return Err(format!(
                "GATEWAY_CONTEXT_WINDOW={window_tokens} tokens leaves {usable_tokens} after reserving {reply_reserve_tokens} for the reply (GATEWAY_MAX_TOKENS plus GATEWAY_THINKING_BUDGET); the agent needs at least {} tokens of context",
                MIN_CONTEXT_CHARS.div_ceil(CHARS_PER_TOKEN as usize)
            ));
        }
        Ok(Self {
            max_turns,
            max_context_chars,
            max_part_chars: MAX_PART_CHARS.min(max_context_chars / PARTS_PER_CONTEXT),
            ..Self::default()
        })
    }
}

#[derive(Debug, Clone)]
pub struct ToolEvidence {
    /// 1-based turn that produced it.
    pub turn: u32,
    /// Wire call id: the model's own, or a synthesized `call_<turn>_<index>`
    /// when the model omitted one. Never empty.
    pub id: String,
    /// Executed tool name.
    pub tool: String,
    /// What the model saw: the rendered result, with the truncation marker
    /// when a non-fetch result was capped at `max_part_chars`.
    pub excerpt: String,
    /// `Some` when the tool yields a source URL.
    pub url: Option<String>,
    /// Engine that produced fetched Evidence (`static`/`browser`); `None`
    /// for everything that is not a successful fetch.
    pub fetch_path: Option<FetchPath>,
    /// The whole fetched page, set only on the call that fetched it over the
    /// network (#80). `excerpt` stays what the model saw (one part); the
    /// Session persists this page once per URL. Continuation parts and
    /// repeat fetches carry `None`.
    pub page: Option<Arc<Evidence>>,
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
    /// Detail of the most recent `search` call that came back
    /// `SearchProviderError::AllFailed { all_challenged: true, .. }` (#53):
    /// every leg was a bot-wall Challenge, never a usable results page.
    /// `None` when no search call in the run was fully walled.
    pub search_blocked: Option<String>,
    /// Whether any `search` call in the run ever returned a non-empty Hit
    /// list. `run_research` uses this together with `search_blocked` and
    /// zero fetched Evidence to decide whether a "successful" FINAL is
    /// actually a walled run the Harness must see as a failure.
    pub had_search_hits: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::GatewayError;

    /// Whatever the window and reserve, a budget that is handed out can be
    /// worked in: a part is never empty, never above its ceiling, never
    /// more than an eighth of the context, and the context never passes
    /// its ceiling. Anything else is an error that names the variable.
    #[test]
    fn for_window_hands_out_only_workable_budgets() {
        let windows = [0, 1_000, 8_192, 10_000, 32_000, 128_000, 200_000, u32::MAX];
        let reserves = [0, 4_096, 8_192, 40_000, u32::MAX];
        for window in windows {
            for reserve in reserves {
                match LoopBudget::for_window(5, window, reserve) {
                    Ok(budget) => {
                        assert_eq!(budget.max_turns, 5);
                        assert!(budget.max_context_chars >= MIN_CONTEXT_CHARS);
                        assert!(budget.max_context_chars <= MAX_CONTEXT_CHARS_CEILING);
                        assert!(budget.max_part_chars >= 1);
                        assert!(budget.max_part_chars <= MAX_PART_CHARS);
                        assert!(
                            budget.max_part_chars * PARTS_PER_CONTEXT <= budget.max_context_chars
                        );
                    }
                    Err(reason) => assert!(
                        reason.contains("GATEWAY_CONTEXT_WINDOW"),
                        "window {window}, reserve {reserve}: {reason}"
                    ),
                }
            }
        }
    }

    /// A bigger window never shrinks the budget, and a bigger reply
    /// reserve never grows it.
    #[test]
    fn for_window_grows_with_the_window_and_shrinks_with_the_reserve() {
        let chars = |window: u32, reserve: u32| {
            LoopBudget::for_window(8, window, reserve)
                .expect("workable budget")
                .max_context_chars
        };
        assert!(chars(60_000, 8_192) > chars(40_000, 8_192));
        assert!(chars(60_000, 8_192) > chars(60_000, 20_000));
        // Past the ceiling the window stops mattering.
        assert_eq!(chars(500_000, 8_192), chars(1_000_000, 8_192));
    }

    /// The reserve is subtracted before anything is sized: a window that
    /// the reply reserve swallows is an error, not a zero-sized budget.
    #[test]
    fn for_window_rejects_a_window_the_reply_reserve_swallows() {
        let err = LoopBudget::for_window(8, 8_192, 8_192).expect_err("nothing left");
        assert!(err.contains("GATEWAY_CONTEXT_WINDOW=8192"), "{err}");
        assert!(LoopBudget::for_window(8, 1_000, 8_192).is_err());
        assert!(LoopBudget::for_window(8, 0, 0).is_err());
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
