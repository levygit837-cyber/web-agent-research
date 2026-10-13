//! Conservative repair turn for ungrounded bullets (#125).
//!
//! After the final answer is parsed and checked ([`verify_synthesis`]),
//! bullets whose quote is missing from the page they cite (`none`) or whose
//! values are not all on it (`partial`) are sent back once, with tools off.
//! The model may only swap a bullet's quote for a verbatim quote from the
//! same cited page, or drop its link. [`only_quotes_and_links_changed`]
//! enforces that in code: a repair that changes any bullet's claim text, or
//! adds, drops or reorders a bullet, theme or link target, is discarded. The
//! repaired answer is checked again and replaces the original only when it is
//! better ([`is_better`]). The turn counts in `turns_used`, and
//! it never runs past the turn budget or into the deadline's final reserve.
//! A repair that fails in any way keeps the original answer: the run already
//! has a usable one.

use std::collections::HashSet;

use crate::llm::{Gateway, TokenUsage, ToolChoice, ToolDef};
use crate::research::agent_loop::answer::{
    parse_answer, retain_fetched_citations, strip_links, ANSWER_CLOSE, ANSWER_OPEN,
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

/// Bullets that cite a fetched page without full support, as the repair
/// message lists them. Uncited bullets are not listed: the repair may not add
/// a link, so it could do nothing for them.
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
        .filter(|bullet| matches!(bullet.support, Support::None | Support::Partial))
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
        Support::Partial | Support::Unchecked | Support::Exact => {
            "the quote is on the cited page, but a number, version, error code or identifier in the bullet is not"
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
        "\nTools are off. Reply with the complete answer inside {ANSWER_OPEN}{ANSWER_CLOSE} tags, in the same format as before. Copy the summary, the theme headings, every bullet's wording and every link exactly as they are. You may change only two things, and only on the flagged bullets:\n\
- Replace its `(quote: \"…\")` with a sentence copied word for word from the page that bullet already cites.\n\
- Remove its link, when that page does not state its point; the bullet then stays in the answer without a source.\n\
Do not reword, add or delete any bullet."
    ));
    text
}

/// Whether `repaired` differs from `original` only in bullet quotes and in
/// dropped links: same summary, same themes and bullet count, same claim text
/// per bullet once links are set aside, and no link to a page the bullet did
/// not already cite.
pub(super) fn only_quotes_and_links_changed(original: &Synthesis, repaired: &Synthesis) -> bool {
    strip_links(&original.summary) == strip_links(&repaired.summary)
        && original.themes.len() == repaired.themes.len()
        && original.themes.iter().zip(&repaired.themes).all(|(a, b)| {
            a.title == b.title
                && a.points.len() == b.points.len()
                && a.points.iter().zip(&b.points).all(|(x, y)| {
                    strip_links(&x.text) == strip_links(&y.text)
                        && y.links.iter().all(|link| x.links.contains(link))
                })
        })
}

/// Run the repair turn when it is wanted and fits, and return the better of
/// the two answers (the repaired one only if the guard accepts it). `answer_turn` is the number of the turn that wrote
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
    if !only_quotes_and_links_changed(&original, &repaired) {
        tracing::warn!("grounding repair changed more than quotes and links; keeping the original");
        return keep(original, 1, usage);
    }
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

    fn parsed(body: &str) -> Synthesis {
        parse_answer(body, crate::research::synthesis::SynthesisSize::Medium).expect("parses")
    }

    #[test]
    fn only_none_and_partial_bullets_are_flagged() {
        let mut synthesis = parsed(
            "<answer>\nSummary.\n\n## T\n- One ([A](https://a.example/)).\n- Two ([A](https://a.example/)).\n- Three ([A](https://a.example/)).\n- Four.\n</answer>",
        );
        synthesis.themes[0].points[0].support = Support::Exact;
        synthesis.themes[0].points[1].support = Support::Partial;
        synthesis.themes[0].points[2].support = Support::None;
        let list = flagged(&synthesis);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].support, Support::Partial);
        assert_eq!(list[1].support, Support::None);
        let message = repair_message(&list);
        assert!(message.contains("1. [T] Two"));
        assert!(message.contains("2. [T] Three"));
        assert!(!message.contains("One"));
        assert!(!message.contains("Four"));
    }

    const ORIGINAL: &str = "<answer>\nSummary ([A](https://a.example/)).\n\n## T\n- Rust 1.75 added AFIT ([A](https://a.example/)) (quote: \"made up\").\n- Second claim ([A](https://a.example/)).\n</answer>";

    #[test]
    fn a_quote_swap_is_accepted() {
        let repaired = parsed(
            "<answer>\nSummary ([A](https://a.example/)).\n\n## T\n- Rust 1.75 added AFIT ([A](https://a.example/)) (quote: \"Rust 1.75 stabilizes AFIT\").\n- Second claim ([A](https://a.example/)).\n</answer>",
        );
        assert!(only_quotes_and_links_changed(&parsed(ORIGINAL), &repaired));
    }

    #[test]
    fn a_dropped_link_is_accepted() {
        let repaired = parsed(
            "<answer>\nSummary ([A](https://a.example/)).\n\n## T\n- Rust 1.75 added AFIT.\n- Second claim ([A](https://a.example/)).\n</answer>",
        );
        assert!(only_quotes_and_links_changed(&parsed(ORIGINAL), &repaired));
    }

    #[test]
    fn a_changed_bullet_text_is_rejected() {
        let repaired = parsed(
            "<answer>\nSummary ([A](https://a.example/)).\n\n## T\n- Rust 1.76 added AFIT ([A](https://a.example/)) (quote: \"Rust 1.76 stabilizes AFIT\").\n- Second claim ([A](https://a.example/)).\n</answer>",
        );
        assert!(!only_quotes_and_links_changed(&parsed(ORIGINAL), &repaired));
    }

    #[test]
    fn a_dropped_bullet_is_rejected() {
        let repaired = parsed(
            "<answer>\nSummary ([A](https://a.example/)).\n\n## T\n- Second claim ([A](https://a.example/)).\n</answer>",
        );
        assert!(!only_quotes_and_links_changed(&parsed(ORIGINAL), &repaired));
    }

    #[test]
    fn an_added_bullet_is_rejected() {
        let repaired = parsed(
            "<answer>\nSummary ([A](https://a.example/)).\n\n## T\n- Rust 1.75 added AFIT ([A](https://a.example/)) (quote: \"made up\").\n- Second claim ([A](https://a.example/)).\n- Third ([A](https://a.example/)).\n</answer>",
        );
        assert!(!only_quotes_and_links_changed(&parsed(ORIGINAL), &repaired));
    }

    #[test]
    fn a_new_link_target_is_rejected() {
        let repaired = parsed(
            "<answer>\nSummary ([A](https://a.example/)).\n\n## T\n- Rust 1.75 added AFIT ([B](https://b.example/)) (quote: \"made up\").\n- Second claim ([A](https://a.example/)).\n</answer>",
        );
        assert!(!only_quotes_and_links_changed(&parsed(ORIGINAL), &repaired));
    }
}
