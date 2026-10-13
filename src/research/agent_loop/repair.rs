//! Repair turn for ungrounded bullets (#125).
//!
//! After the final answer is parsed and checked ([`verify_synthesis`]),
//! bullets that are `none`, `partial`, uncited, or cite a page the run never
//! fetched are sent back once, with their check result and tools off: the
//! model rewrites each from the cited page with a verbatim quote, or drops
//! it. The repaired answer is checked again and replaces the original only
//! when it is better ([`is_better`]). The turn counts in `turns_used`, and
//! it never runs past the turn budget or into the deadline's final reserve.
//! A repair that fails in any way keeps the original answer: the run already
//! has a usable one.

use std::collections::HashSet;

use crate::llm::{Gateway, TokenUsage, ToolChoice, ToolDef};
use crate::research::agent_loop::answer::{
    parse_answer, retain_fetched_citations, ANSWER_CLOSE, ANSWER_OPEN,
};
use crate::research::agent_loop::context::{self, HistoryEntry};
use crate::research::agent_loop::types::{LoopBudget, LoopInput, ToolEvidence};
use crate::research::agent_loop::verify::verify_synthesis;
use crate::research::synthesis::{Support, Synthesis, Verification};
use crate::web::search::dedup_key;

/// Keep only citations of pages fetched in this run, then grade every bullet
/// against those pages.
pub(super) fn ground(synthesis: &mut Synthesis, evidence: &[ToolEvidence]) {
    let fetched: HashSet<String> = evidence
        .iter()
        .filter_map(|item| item.url.as_deref())
        .map(dedup_key)
        .collect();
    retain_fetched_citations(synthesis, &fetched);
    verify_synthesis(synthesis, evidence);
}

/// What the repair turn did to the run.
pub(super) struct Repaired {
    /// The answer to return: the repaired one when it is better, else the
    /// original, untouched.
    pub(super) synthesis: Synthesis,
    /// Gateway calls the repair made (0 or 1), to add to `turns_used`.
    pub(super) turns: u32,
    pub(super) usage: TokenUsage,
}

/// Bullets that are not fully supported, as the repair message lists them.
struct Flagged<'a> {
    theme: &'a str,
    text: &'a str,
    quote: Option<&'a str>,
    support: Support,
}

fn flagged(synthesis: &Synthesis) -> Vec<Flagged<'_>> {
    synthesis
        .themes
        .iter()
        .flat_map(|theme| {
            theme.points.iter().map(move |point| Flagged {
                theme: theme.title.as_str(),
                text: point.text.as_str(),
                quote: point.quote.as_deref(),
                support: point.support,
            })
        })
        .filter(|bullet| bullet.support != Support::Exact)
        .collect()
}

/// Bullets with no fetched-page support at all: `none`, uncited, or citing a
/// page the run never fetched.
fn unsupported(verification: &Verification) -> u32 {
    verification.bullets - verification.cited + verification.none
}

/// Whether `repaired` replaces `original`: fewer unsupported bullets, or the
/// same number and fewer `partial` ones, and never fewer `exact` ones.
pub(super) fn is_better(original: &Verification, repaired: &Verification) -> bool {
    if repaired.exact < original.exact {
        return false;
    }
    let (before, after) = (unsupported(original), unsupported(repaired));
    after < before || (after == before && repaired.partial < original.partial)
}

fn check_result(support: Support) -> &'static str {
    match support {
        Support::None => "its quote is missing, or it is not on the cited page",
        Support::Partial => {
            "the quote is on the cited page, but a number, version, error code or identifier in the bullet is not"
        }
        Support::Unchecked | Support::Exact => {
            "it cites no page you fetched in this run (no link, or a link to a page you did not fetch)"
        }
    }
}

fn repair_message(flagged: &[Flagged<'_>]) -> String {
    let mut text = String::from(
        "A check of your answer against the pages you fetched flagged these bullets:\n",
    );
    for (index, bullet) in flagged.iter().enumerate() {
        let quote = bullet
            .quote
            .map(|quote| format!(" (quote: \"{quote}\")"))
            .unwrap_or_default();
        text.push_str(&format!(
            "{}. [{}] {}{}\n   Check: {}.\n",
            index + 1,
            bullet.theme,
            bullet.text,
            quote,
            check_result(bullet.support),
        ));
    }
    text.push_str(&format!(
        "\nTools are off. Reply with the complete corrected answer inside {ANSWER_OPEN}{ANSWER_CLOSE} tags, in the same format as before. Change only the flagged bullets: copy the summary, the theme headings and every other bullet unchanged.\n\
For each flagged bullet, in this order of preference:\n\
- Keep it and fix it from the page it cites: find the sentence on that page that states its point and copy it word for word as the `(quote: \"…\")`. If a number, version, flag or default in the bullet is not on that page, correct it to what the page says or remove only that value; keep the rest of the bullet and every other value that is on the page.\n\
- If it cites no page you fetched, or its point is on another page you fetched, cite that page and quote it.\n\
- Drop it only when none of the pages you fetched states its point.\n\
Add no new claim. When an official source and a secondary source disagree, state the official one."
    ));
    text
}

/// Run the repair turn when it is wanted and fits, and return the better of
/// the two answers. `answer_turn` is the number of the turn that wrote
/// `original` and `answer_text` its raw reply. Never errors: a gateway
/// error, a reply with tool calls, or an unparsable reply keeps `original`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn repair(
    gateway: &Gateway,
    defs: &[ToolDef],
    system_prompt: &str,
    input: &LoopInput,
    history: &[HistoryEntry],
    budget: &LoopBudget,
    evidence: &[ToolEvidence],
    original: Synthesis,
    answer_text: &str,
    answer_turn: u32,
    turn_budget: u32,
) -> Repaired {
    let keep = |synthesis: Synthesis, turns: u32, usage: TokenUsage| Repaired {
        synthesis,
        turns,
        usage,
    };
    let has_pages = evidence.iter().any(|item| item.page.is_some());
    let flagged_bullets = flagged(&original);
    if !has_pages
        || defs.is_empty()
        || flagged_bullets.is_empty()
        || answer_turn >= turn_budget
        || budget.in_final_reserve()
    {
        return keep(original, 0, TokenUsage::default());
    }
    let mut with_answer = history.to_vec();
    with_answer.push(HistoryEntry::TextTurn {
        assistant: answer_text.to_owned(),
        observation: repair_message(&flagged_bullets),
    });
    let messages = context::assemble(system_prompt, input, &with_answer, budget);
    let reply = match gateway
        .chat_with_tools(&messages, defs, Some(ToolChoice::None))
        .await
    {
        Ok(reply) => reply,
        Err(err) => {
            tracing::warn!("grounding repair turn failed; keeping the original answer: {err}");
            return keep(original, 0, TokenUsage::default());
        }
    };
    let usage = reply.usage.clone();
    if !reply.tool_calls.is_empty() {
        tracing::warn!("grounding repair turn called tools; keeping the original answer");
        return keep(original, 1, usage);
    }
    let Ok(mut repaired) = parse_answer(&reply.output, input.size) else {
        return keep(original, 1, usage);
    };
    ground(&mut repaired, evidence);
    if is_better(&original.verification, &repaired.verification) {
        keep(repaired, 1, usage)
    } else {
        keep(original, 1, usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verification(bullets: u32, cited: u32, exact: u32, partial: u32, none: u32) -> Verification {
        Verification {
            bullets,
            cited,
            exact,
            partial,
            none,
        }
    }

    #[test]
    fn fewer_unsupported_bullets_with_the_same_exact_is_better() {
        let original = verification(4, 3, 2, 0, 1);
        // The uncited bullet and the `none` bullet are gone.
        assert!(is_better(&original, &verification(2, 2, 2, 0, 0)));
    }

    #[test]
    fn more_exact_bullets_with_fewer_unsupported_is_better() {
        let original = verification(3, 3, 1, 0, 2);
        assert!(is_better(&original, &verification(3, 3, 2, 0, 1)));
    }

    #[test]
    fn fewer_exact_bullets_is_never_better() {
        let original = verification(4, 4, 3, 0, 1);
        assert!(!is_better(&original, &verification(2, 2, 2, 0, 0)));
    }

    #[test]
    fn equal_answers_keep_the_original() {
        let original = verification(3, 3, 2, 0, 1);
        assert!(!is_better(&original, &original));
    }

    #[test]
    fn a_dropped_partial_bullet_is_better_when_nothing_else_changes() {
        let original = verification(3, 3, 2, 1, 0);
        assert!(is_better(&original, &verification(2, 2, 2, 0, 0)));
    }

    #[test]
    fn more_unsupported_bullets_is_not_better() {
        let original = verification(3, 3, 2, 1, 0);
        assert!(!is_better(&original, &verification(3, 3, 2, 0, 1)));
    }

    #[test]
    fn exact_bullets_are_not_flagged() {
        let mut synthesis = parse_answer(
            "<answer>\nSummary.\n\n## T\n- One ([A](https://a.example/)).\n- Two ([A](https://a.example/)).\n- Three.\n</answer>",
            crate::research::synthesis::SynthesisSize::Medium,
        )
        .expect("parses");
        synthesis.themes[0].points[0].support = Support::Exact;
        synthesis.themes[0].points[1].support = Support::Partial;
        let list = flagged(&synthesis);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].support, Support::Partial);
        assert_eq!(list[1].support, Support::Unchecked);
        let message = repair_message(&list);
        assert!(message.contains("1. [T] Two"));
        assert!(message.contains("2. [T] Three."));
        assert!(!message.contains("One"));
    }
}
