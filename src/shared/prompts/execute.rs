//! Executor prompt: drives the `agent_loop/` until synthesis.
//!
//! Follows the agent anatomy: short role + constraints + workflow, with tool
//! knowledge as a referenced roster section. JSON Schemas travel via
//! `ToolDef` on the wire and are never pasted here.

use crate::shared::types::synthesis::SynthesisSize;

pub(crate) const EXECUTE_ROLE_TEXT: &str = "You are a multi-turn web-research executor. \
Plan the research, then search broadly, read the promising hits, and synthesize the evidence into a final answer. \
You run direct-Execute only: never ask clarifying questions, start working with the offered tools.";

fn size_guidance(size: SynthesisSize) -> &'static str {
    match size {
        SynthesisSize::Small => "summary plus a single theme with at least one citation",
        SynthesisSize::Medium => "summary plus 1-4 themed sections",
        SynthesisSize::Large => "summary plus multi-point themed sections",
        SynthesisSize::Deep => "summary plus multi-point themed sections (deep shape)",
    }
}

/// Build the system prompt for one run: role, tool-use discipline naming
/// each allowed tool with a one-line purpose, workflow, and the synthesis
/// contract for the requested size.
pub fn build_system_prompt(tool_roster: &[(&str, &str)], size: SynthesisSize) -> String {
    let mut prompt = String::new();
    prompt.push_str(EXECUTE_ROLE_TEXT);
    prompt.push_str("\n\nInstructions:\n");
    prompt.push_str("- Use the offered tool calls to gather evidence before answering; never invent URLs — only fetch URLs returned by search.\n");
    prompt.push_str("- Cite evidence with inline [title](url) links in the final answer.\n");
    prompt.push_str("- Organize the final answer with `## ` headings, one per theme.\n");
    prompt.push_str(
        "- Answer directly with plain text and no tool call when the evidence suffices.\n",
    );
    prompt.push_str("\nWorkflow:\n");
    prompt.push_str("1. Search broadly for the research goal.\n");
    prompt.push_str("2. Fetch the promising hits and read them.\n");
    prompt.push_str("3. Repeat search and fetch until the evidence suffices.\n");
    prompt.push_str("4. Write the final answer as plain text with no tool call, then stop.\n");
    prompt.push_str("\nAvailable tools:\n");
    if tool_roster.is_empty() {
        prompt.push_str("- (none — answer directly)\n");
    } else {
        for (name, purpose) in tool_roster {
            prompt.push_str(&format!("- {name} — {purpose}\n"));
        }
    }
    prompt.push_str(&format!(
        "\nRequested size: {} — {}.\nA non-empty text answer with no tool calls ends the run.\n",
        size.as_str(),
        size_guidance(size)
    ));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Vec<(&'static str, &'static str)> {
        vec![
            ("search", "Search the web for pages matching a query."),
            ("fetch", "Fetch a URL and return its content as markdown."),
        ]
    }

    #[test]
    fn golden_text_carries_role_roster_size_and_discipline() {
        let prompt = build_system_prompt(&roster(), SynthesisSize::Medium);
        assert!(
            prompt.contains(EXECUTE_ROLE_TEXT),
            "role text must be present"
        );
        for (name, purpose) in roster() {
            let line = format!("{name} — {purpose}");
            assert!(prompt.contains(&line), "roster line missing: {line}");
        }
        assert!(
            prompt.contains("Requested size: medium"),
            "size line missing"
        );
        assert!(prompt.contains("tool call"), "tool call discipline missing");
        assert!(prompt.contains("## "), "heading discipline missing");
        assert!(
            prompt.contains("[title](url)"),
            "citation discipline missing"
        );
    }

    #[test]
    fn size_line_echoes_requested_size() {
        for size in [
            SynthesisSize::Small,
            SynthesisSize::Medium,
            SynthesisSize::Large,
            SynthesisSize::Deep,
        ] {
            let prompt = build_system_prompt(&roster(), size);
            assert!(
                prompt.contains(&format!("Requested size: {}", size.as_str())),
                "size line must echo {}",
                size.as_str()
            );
        }
    }

    #[test]
    fn empty_roster_stays_explicit() {
        let prompt = build_system_prompt(&[], SynthesisSize::Small);
        assert!(prompt.contains("(none — answer directly)"));
        assert!(prompt.contains("Requested size: small"));
    }
}
