//! Message assembly and truncation for the agent loop.
//!
//! The loop keeps a text transcript: prior assistant outputs verbatim plus
//! `user` observations quoting each tool call with its result. Truncation is
//! char-based (no tokenizer): evidence texts are pre-capped at insertion and
//! oldest history pairs drop first when the assembled messages exceed
//! `max_context_chars`. `messages[0]` (system) and `messages[1]` (goal) are
//! never dropped.

use crate::shared::agent_loop::runner::LoopBudget;
use crate::shared::agent_loop::runner::LoopInput;
use crate::shared::gateway::ChatMessage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryEntry {
    Assistant(String),
    Observation(String),
}

impl HistoryEntry {
    fn chars(&self) -> usize {
        match self {
            Self::Assistant(text) | Self::Observation(text) => text.chars().count(),
        }
    }

    fn is_observation(&self) -> bool {
        matches!(self, Self::Observation(_))
    }

    #[cfg(test)]
    pub(crate) fn assistant_for_test(text: String) -> Self {
        Self::Assistant(text)
    }

    #[cfg(test)]
    pub(crate) fn observation_for_test(text: String) -> Self {
        Self::Observation(text)
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
pub(crate) fn cap_evidence(text: &str, max_evidence_chars: usize) -> String {
    let total: usize = text.chars().count();
    if total <= max_evidence_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_evidence_chars).collect();
    let removed = total - max_evidence_chars;
    format!("{kept}\n[truncated {removed} chars]")
}

fn total_chars(system_prompt: &str, goal: &str, history: &[HistoryEntry]) -> usize {
    system_prompt.chars().count()
        + goal.chars().count()
        + history.iter().map(HistoryEntry::chars).sum::<usize>()
}

/// Drop oldest assistant/observation pairs until the assembled transcript
/// fits `max_context_chars` (or one pair remains). The marker prepends to
/// the oldest surviving observation.
pub(crate) fn truncate_history(
    system_chars: usize,
    goal_chars: usize,
    history: &[HistoryEntry],
    budget: &LoopBudget,
) -> (Vec<HistoryEntry>, bool) {
    let mut surviving = history.to_vec();
    let mut dropped_turns = 0_usize;
    while surviving.len() > 2
        && system_chars + goal_chars + surviving.iter().map(HistoryEntry::chars).sum::<usize>()
            > budget.max_context_chars
    {
        surviving.drain(..2);
        dropped_turns += 1;
    }
    if dropped_turns == 0 {
        return (surviving, false);
    }
    let marker = format!("[history truncated: {dropped_turns} oldest turn(s) dropped]\n");
    for entry in &mut surviving {
        if entry.is_observation() {
            let text = match entry {
                HistoryEntry::Observation(text) => std::mem::take(text),
                HistoryEntry::Assistant(_) => unreachable!("observation checked above"),
            };
            *entry = HistoryEntry::Observation(format!("{marker}{text}"));
            break;
        }
    }
    (surviving, true)
}

/// Assemble the wire messages: system prompt, goal, then surviving history.
/// Empty assistant outputs are skipped; `was_truncated` is loop telemetry.
pub(crate) fn assemble(
    system_prompt: &str,
    input: &LoopInput,
    history: &[HistoryEntry],
    budget: &LoopBudget,
) -> Vec<ChatMessage> {
    let goal = goal_message(input);
    let system_chars = system_prompt.chars().count();
    let goal_chars = goal.chars().count();
    let (surviving, was_truncated) = truncate_history(system_chars, goal_chars, history, budget);
    if was_truncated {
        tracing::info!("agent loop history truncated to fit context budget");
    }
    let mut messages = Vec::with_capacity(2 + surviving.len());
    messages.push(ChatMessage {
        role: "system".to_owned(),
        content: system_prompt.to_owned(),
    });
    messages.push(ChatMessage {
        role: "user".to_owned(),
        content: goal,
    });
    for entry in surviving {
        match entry {
            HistoryEntry::Assistant(text) => {
                if !text.is_empty() {
                    messages.push(ChatMessage {
                        role: "assistant".to_owned(),
                        content: text,
                    });
                }
            }
            HistoryEntry::Observation(text) => messages.push(ChatMessage {
                role: "user".to_owned(),
                content: text,
            }),
        }
    }
    let _ = total_chars(system_prompt, &goal_message(input), history);
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::synthesis::SynthesisSize;

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

    fn pair(n: usize) -> Vec<HistoryEntry> {
        vec![
            HistoryEntry::Assistant(format!("thinking {n}")),
            HistoryEntry::Observation(format!("TOOL RESULTS:\nresult {n}")),
        ]
    }

    #[test]
    fn layout_is_system_goal_then_history() {
        let mut history = Vec::new();
        history.extend(pair(1));
        let messages = assemble("sys", &input(), &history, &LoopBudget::default());
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "sys");
        assert_eq!(messages[1].role, "user");
        assert!(messages[1].content.contains("What is the Obscura engine?"));
        assert!(messages[1].content.contains("medium"));
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[3].role, "user");
    }

    #[test]
    fn empty_history_is_exactly_two_messages() {
        let messages = assemble("sys", &input(), &[], &LoopBudget::default());
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn empty_assistant_output_is_skipped() {
        let history = vec![
            HistoryEntry::Assistant(String::new()),
            HistoryEntry::Observation("TOOL RESULTS:\n[x search]: ok".to_owned()),
        ];
        let messages = assemble("sys", &input(), &history, &LoopBudget::default());
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, "user");
    }

    #[test]
    fn truncation_drops_oldest_and_marks_survivor() {
        let mut history = Vec::new();
        for n in 1..=4 {
            history.extend(pair(n));
        }
        let full = assemble("sys", &input(), &history, &LoopBudget::default());
        assert_eq!(full.len(), 2 + 8);
        let tiny = budget_for(120);
        let messages = assemble("sys", &input(), &history, &tiny);
        assert_eq!(messages[0].content, "sys");
        assert!(messages[1].content.contains("What is the Obscura engine?"));
        assert!(
            messages.len() < full.len(),
            "truncation must shrink the transcript"
        );
        let oldest_observation = messages
            .iter()
            .skip(2)
            .find(|message| message.role == "user")
            .expect("one observation must survive");
        assert!(
            oldest_observation
                .content
                .starts_with("[history truncated:"),
            "marker missing: {}",
            oldest_observation.content
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.content.contains("result 1")),
            "oldest pair must drop first"
        );
    }

    #[test]
    fn single_pair_survives_even_over_cap() {
        let history = pair(1);
        let tiny = budget_for(10);
        let messages = assemble("sys", &input(), &history, &tiny);
        assert!(messages
            .iter()
            .any(|message| message.content.contains("result 1")));
    }

    #[test]
    fn cap_evidence_marks_removed_count() {
        let text = "abcdefghij";
        assert_eq!(cap_evidence(text, 10), "abcdefghij");
        assert_eq!(cap_evidence(text, 4), "abcd\n[truncated 6 chars]");
    }
}
