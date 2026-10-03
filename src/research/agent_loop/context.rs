//! Message assembly and truncation for the agent loop.
//!
//! History keeps one entry per completed turn. A text turn pairs the
//! model's own (optionally empty) commentary with a repair observation —
//! used for the empty-answer repair path. A tool turn carries the native
//! wire shape: one assistant message with `tool_calls` exactly as the model
//! sent them (ids preserved, or synthesized when the model omitted one) and
//! one `tool` message per call, in wire order. Truncation is char-based (no
//! tokenizer): evidence texts are pre-capped at insertion, and turns drop
//! oldest-first as a whole entry, so a tool turn's assistant message and all
//! its `tool` messages leave together — no orphan `tool` message or
//! unpaired `tool_calls` ever survives. `messages[0]` (system) and
//! `messages[1]` (goal) are never dropped.

use crate::llm::{ChatMessage, ReplayBlocks, RequestedToolCall};
use crate::research::agent_loop::runner::LoopBudget;
use crate::research::agent_loop::runner::LoopInput;

/// One `tool`-role wire message: the id it answers plus rendered content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolMessage {
    pub(crate) id: String,
    pub(crate) content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryEntry {
    /// Model commentary (skipped when empty) plus a repair nudge, for the
    /// empty-answer repair path. Never used for tool turns.
    TextTurn {
        assistant: String,
        observation: String,
    },
    /// One tool turn, dropped as a whole by truncation: the assistant
    /// `tool_calls` message (ids resolved) and its paired `tool` messages,
    /// in wire order. `replay` carries the provider blocks the assistant
    /// message must echo verbatim (Anthropic thinking); empty on OpenAI.
    ToolTurn {
        text: String,
        calls: Vec<RequestedToolCall>,
        results: Vec<ToolMessage>,
        replay: ReplayBlocks,
    },
}

impl HistoryEntry {
    fn chars(&self) -> usize {
        match self {
            Self::TextTurn {
                assistant,
                observation,
            } => assistant.chars().count() + observation.chars().count(),
            Self::ToolTurn {
                text,
                calls,
                results,
                replay,
            } => {
                text.chars().count()
                    + replay.chars()
                    + calls
                        .iter()
                        .map(|call| {
                            call.id.chars().count()
                                + call.name.chars().count()
                                + call.arguments.chars().count()
                        })
                        .sum::<usize>()
                    + results
                        .iter()
                        .map(|result| result.id.chars().count() + result.content.chars().count())
                        .sum::<usize>()
            }
        }
    }
}

pub(crate) fn goal_message(input: &LoopInput) -> String {
    format!(
        "Research goal: {}\nRequested synthesis size: {}.",
        input.goal,
        input.size.as_str()
    )
}

/// Cap one evidence text at insertion time; excess becomes a marker naming
/// the removed char count.
pub(crate) fn cap_evidence(text: &str, max_chars: usize) -> String {
    let total: usize = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_chars).collect();
    let removed = total - max_chars;
    format!("{kept}\n[truncated {removed} chars]")
}

/// Split a fetched page into parts of at most `part_chars` chars (minimum 1),
/// always on char boundaries. A cut prefers the last `\n` inside the final
/// 10% of the window, so a part usually ends at a line end; with none there,
/// it cuts at exactly `part_chars` chars. The parts concatenate back to
/// `markdown` byte for byte, and an empty page is one empty part, so the
/// result is never empty.
pub(crate) fn page_parts(markdown: &str, part_chars: usize) -> Vec<&str> {
    let part_chars = part_chars.max(1);
    let tail_chars = part_chars - part_chars / 10;
    let mut parts = Vec::new();
    let mut rest = markdown;
    loop {
        let Some((window_end, _)) = rest.char_indices().nth(part_chars) else {
            parts.push(rest);
            return parts;
        };
        let window = &rest[..window_end];
        let tail_start = window
            .char_indices()
            .nth(tail_chars)
            .map_or(window_end, |(byte, _)| byte);
        let cut = window[tail_start..]
            .rfind('\n')
            .map_or(window_end, |newline| tail_start + newline + 1);
        let (part, remainder) = rest.split_at(cut);
        parts.push(part);
        rest = remainder;
    }
}

/// Drop oldest turns until the assembled transcript fits `max_context_chars`
/// (or one turn remains). Each `HistoryEntry` is one turn, dropped whole, so
/// a tool turn's assistant `tool_calls` message and all its `tool` messages
/// always leave together. Returns the surviving entries and how many turns
/// were dropped (0 when nothing was).
pub(crate) fn truncate_history(
    system_chars: usize,
    goal_chars: usize,
    history: &[HistoryEntry],
    budget: &LoopBudget,
) -> (Vec<HistoryEntry>, usize) {
    let mut surviving = history.to_vec();
    let mut dropped_turns = 0_usize;
    while surviving.len() > 1
        && system_chars + goal_chars + surviving.iter().map(HistoryEntry::chars).sum::<usize>()
            > budget.max_context_chars
    {
        surviving.remove(0);
        dropped_turns += 1;
    }
    (surviving, dropped_turns)
}

/// Assemble the wire messages: system prompt, goal, an optional truncation
/// note, then surviving history. A text turn skips its assistant message
/// when empty; a tool turn always emits its assistant `tool_calls` message
/// (empty commentary serializes as wire `content: null`) followed by its
/// `tool` messages. `was_truncated` telemetry logs when turns were dropped.
pub(crate) fn assemble(
    system_prompt: &str,
    input: &LoopInput,
    history: &[HistoryEntry],
    budget: &LoopBudget,
) -> Vec<ChatMessage> {
    let goal = goal_message(input);
    let system_chars = system_prompt.chars().count();
    let goal_chars = goal.chars().count();
    let (surviving, dropped_turns) = truncate_history(system_chars, goal_chars, history, budget);
    if dropped_turns > 0 {
        tracing::info!(
            "agent loop history truncated: {dropped_turns} oldest turn(s) dropped to fit context budget"
        );
    }
    let mut messages = Vec::with_capacity(3 + surviving.len() * 2);
    messages.push(ChatMessage::system(system_prompt));
    messages.push(ChatMessage::user(goal));
    if dropped_turns > 0 {
        messages.push(ChatMessage::user(format!(
            "[history truncated: {dropped_turns} oldest turn(s) dropped]"
        )));
    }
    for entry in surviving {
        match entry {
            HistoryEntry::TextTurn {
                assistant,
                observation,
            } => {
                if !assistant.is_empty() {
                    messages.push(ChatMessage::assistant(assistant));
                }
                messages.push(ChatMessage::user(observation));
            }
            HistoryEntry::ToolTurn {
                text,
                calls,
                results,
                replay,
            } => {
                messages.push(ChatMessage::assistant_tool_calls(&text, &calls, &replay));
                for result in results {
                    messages.push(ChatMessage::tool(result.id, result.content));
                }
            }
        }
    }
    messages
}

/// Test-shared invariant: every `tool` message pairs with a preceding
/// assistant `tool_calls` entry sharing its id, and every assistant-declared
/// id gets exactly one `tool` message. Encodes the "never leave an orphan
/// `tool` message or unpaired `tool_calls`" contract; reused by
/// `runner::tests`.
#[cfg(test)]
pub(crate) fn assert_valid_tool_pairing(messages: &[ChatMessage]) {
    let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
    for message in messages {
        if message.role == "assistant" {
            if let Some(calls) = &message.tool_calls {
                for call in calls {
                    assert!(
                        pending.insert(call.id.clone()),
                        "duplicate tool_call id {}",
                        call.id
                    );
                }
            }
        } else if message.role == "tool" {
            let id = message
                .tool_call_id
                .as_deref()
                .expect("tool message must carry tool_call_id");
            assert!(pending.remove(id), "orphan tool message for id {id}");
        }
    }
    assert!(
        pending.is_empty(),
        "unpaired tool_calls left without a tool message: {pending:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::synthesis::SynthesisSize;

    fn input() -> LoopInput {
        LoopInput {
            goal: "What is the Obscura engine?".to_owned(),
            size: SynthesisSize::Medium,
            allowed_tools: vec!["search".to_owned()],
        }
    }

    fn budget_for(max_context_chars: usize) -> LoopBudget {
        LoopBudget {
            max_context_chars,
            ..LoopBudget::default()
        }
    }

    fn call(id: &str) -> RequestedToolCall {
        RequestedToolCall {
            id: id.to_owned(),
            name: "search".to_owned(),
            arguments: r#"{"query":"x"}"#.to_owned(),
        }
    }

    fn tool_turn(n: usize) -> HistoryEntry {
        HistoryEntry::ToolTurn {
            text: format!("thinking {n}"),
            calls: vec![call(&format!("c{n}"))],
            results: vec![ToolMessage {
                id: format!("c{n}"),
                content: format!("result {n}"),
            }],
            replay: ReplayBlocks::default(),
        }
    }

    #[test]
    fn layout_is_system_goal_then_tool_turn() {
        let history = vec![tool_turn(1)];
        let messages = assemble("sys", &input(), &history, &LoopBudget::default());
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content_text(), "sys");
        assert_eq!(messages[1].role, "user");
        assert!(messages[1]
            .content_text()
            .contains("What is the Obscura engine?"));
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[2].content_text(), "thinking 1");
        assert_eq!(
            messages[2].tool_calls.as_ref().expect("tool_calls").len(),
            1
        );
        assert_eq!(messages[2].tool_calls.as_ref().unwrap()[0].id, "c1");
        assert_eq!(messages[3].role, "tool");
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[3].content_text(), "result 1");
        assert_valid_tool_pairing(&messages);
    }

    #[test]
    fn layout_is_system_goal_then_text_turn() {
        let history = vec![HistoryEntry::TextTurn {
            assistant: "thinking".to_owned(),
            observation: "repair note".to_owned(),
        }];
        let messages = assemble("sys", &input(), &history, &LoopBudget::default());
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[2].content_text(), "thinking");
        assert!(messages[2].tool_calls.is_none());
        assert_eq!(messages[3].role, "user");
        assert_eq!(messages[3].content_text(), "repair note");
    }

    #[test]
    fn empty_history_is_exactly_two_messages() {
        let messages = assemble("sys", &input(), &[], &LoopBudget::default());
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn empty_assistant_text_is_skipped_in_text_turn() {
        let history = vec![HistoryEntry::TextTurn {
            assistant: String::new(),
            observation: "repair note".to_owned(),
        }];
        let messages = assemble("sys", &input(), &history, &LoopBudget::default());
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, "user");
    }

    #[test]
    fn empty_text_is_null_content_in_tool_turn() {
        let history = vec![HistoryEntry::ToolTurn {
            text: String::new(),
            calls: vec![call("c1")],
            results: vec![ToolMessage {
                id: "c1".to_owned(),
                content: "result".to_owned(),
            }],
            replay: ReplayBlocks::default(),
        }];
        let messages = assemble("sys", &input(), &history, &LoopBudget::default());
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(
            messages[2].content, None,
            "empty tool-call commentary must serialize as wire null, not an empty string"
        );
        assert!(messages[2].tool_calls.is_some());
    }

    #[test]
    fn truncation_drops_oldest_whole_turns() {
        let history: Vec<HistoryEntry> = (1..=4).map(tool_turn).collect();
        let full = assemble("sys", &input(), &history, &LoopBudget::default());
        assert_eq!(full.len(), 2 + 4 * 2);
        let tiny = budget_for(140);
        let messages = assemble("sys", &input(), &history, &tiny);
        assert!(
            messages.len() < full.len(),
            "truncation must shrink the transcript"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.content_text().contains("result 1")),
            "oldest turn must drop first"
        );
        let marker = messages
            .iter()
            .find(|message| message.content_text().starts_with("[history truncated:"))
            .expect("marker must survive as its own note");
        assert_eq!(marker.role, "user");
        assert_valid_tool_pairing(&messages);
    }

    #[test]
    fn single_turn_survives_even_over_cap() {
        let history = vec![tool_turn(1)];
        let tiny = budget_for(1);
        let messages = assemble("sys", &input(), &history, &tiny);
        assert!(messages
            .iter()
            .any(|message| message.content_text().contains("result 1")));
        assert_valid_tool_pairing(&messages);
    }

    #[test]
    fn cap_evidence_marks_removed_count() {
        let text = "abcdefghij";
        assert_eq!(cap_evidence(text, 10), "abcdefghij");
        assert_eq!(cap_evidence(text, 4), "abcd\n[truncated 6 chars]");
    }

    #[test]
    fn page_parts_tile_the_page_exactly_whatever_the_text() {
        let newline_heavy = "line of text\n".repeat(500);
        let multibyte = "é漢🦀".repeat(400);
        let no_breaks = "x".repeat(1_234);
        for text in [newline_heavy.as_str(), &multibyte, &no_breaks, "", "a"] {
            for part_chars in [1, 7, 100, 999, 10_000] {
                let parts = page_parts(text, part_chars);
                assert_eq!(parts.concat(), text, "part_chars={part_chars}");
                assert!(!parts.is_empty());
                assert!(
                    parts.iter().all(|p| p.chars().count() <= part_chars),
                    "a part exceeded {part_chars} chars"
                );
            }
        }
    }

    #[test]
    fn page_parts_cut_at_a_line_end_in_the_last_tenth_else_at_the_limit() {
        // Newline at char 95 of a 100-char window: inside the last 10%.
        let late = format!("{}\n{}", "a".repeat(95), "b".repeat(50));
        let parts = page_parts(&late, 100);
        assert_eq!(parts, vec![&late[..96], &late[96..]]);

        // Newline at char 10: too early, so the cut is exactly at the limit.
        let early = format!("{}\n{}", "a".repeat(10), "b".repeat(200));
        let parts = page_parts(&early, 100);
        assert_eq!(parts[0].chars().count(), 100);
        assert_eq!(parts.concat(), early);

        // The last newline of the tail wins over an earlier one.
        let two = format!("{}\n{}\n{}", "a".repeat(91), "b".repeat(5), "c".repeat(50));
        let parts = page_parts(&two, 100);
        assert_eq!(parts[0], &two[..98]);
    }

    #[test]
    fn page_parts_keeps_a_page_that_fits_whole_and_an_empty_page_as_one_part() {
        let exact = "y".repeat(50);
        assert_eq!(page_parts(&exact, 50), vec![exact.as_str()]);
        assert_eq!(page_parts("", 50), vec![""]);
    }
}
