//! Executor prompt: drives the `agent_loop/` until synthesis.
//!
//! Built once per run from the offered tools, the requested size and the
//! run's [`LoopBudget`], so it stays byte-identical across turns and the
//! prompt-cache prefix holds; per-turn state (turns left) travels in the
//! newest observation instead (`agent_loop::runner`). JSON Schemas travel
//! via `ToolDef` on the wire and are never pasted here. Evaluation and
//! rationale (#72): `docs/research/agent-prompt.md`.

use crate::research::agent_loop::LoopBudget;
use crate::research::synthesis::SynthesisSize;
use crate::web::fetch::tool::FETCH_TOOL_NAME;
use crate::web::search::tool::SEARCH_TOOL_NAME;

const ROLE: &str = "You are the research executor behind a web-search tool. \
Another AI agent (the caller) sent you the research goal in the first user message. \
The caller reads only your final answer, never the pages you read, so it depends on you to report what those pages say and link to them. \
Work autonomously: never ask clarifying questions.";

/// Rules for runs that can read pages (`fetch` offered).
const EVIDENCE_RULES: &str = "<evidence_rules>
- Evidence is the text of a page you fetched with `fetch` during this run. Search Hits (title, URL, snippet) are only candidates: a snippet is a search engine's excerpt, often outdated or cut mid-sentence, so never state a fact you saw only in a snippet.
- Fetch before you answer. An answer written from snippets alone hands the caller unverified claims with no sources, which defeats the purpose of this tool.
- Cite only pages you fetched. Links to anything else are removed from the citations the caller receives, so they cannot support a claim.
- Never invent or guess a URL. Fetch URLs from search Hits, from the goal itself, or from links inside a page you already fetched.
</evidence_rules>
";

/// Rules for runs where no offered tool reads pages.
const UNREAD_RULES: &str = "<evidence_rules>
- No tool in this run reads web pages, so nothing you report is verified Evidence: say so in the summary and present each claim as unverified. Never call a tool that is not listed above.
</evidence_rules>
";

/// Answer shape per size, matching what `answer::parse_answer` extracts.
fn size_shape(size: SynthesisSize) -> &'static str {
    match size {
        SynthesisSize::Small => "a 1-2 sentence summary and 1 theme with 2-4 bullets",
        SynthesisSize::Medium => "a 2-3 sentence summary and 2-4 themes with 2-5 bullets each",
        SynthesisSize::Large | SynthesisSize::Deep => {
            "a 3-5 sentence summary and 3-6 themes with 3-6 bullets each"
        }
    }
}

/// Fetched pages a run of this size should read before answering.
fn pages_target(size: SynthesisSize) -> &'static str {
    match size {
        SynthesisSize::Small => "1-2",
        SynthesisSize::Medium => "2-4",
        SynthesisSize::Large | SynthesisSize::Deep => "3-6",
    }
}

fn workflow(size: SynthesisSize, budget: &LoopBudget) -> String {
    format!(
        "<workflow>
1. Search: call `search` once with 2-4 queries that approach the goal from different angles (for example the exact error text, the crate or project name, and the question in plain words).
2. Choose: pick the Hits most likely to hold the answer first-hand: official documentation and references (docs.rs for a Rust crate), the project's own site or repository, release notes and changelogs, and standards; then well-known Q&A sites and in-depth articles. For topics that change over time, prefer the newest pages. Skip mirrors of a page you already have, listicles, and Hits whose snippet is off-topic.
3. Fetch: call `fetch` on the chosen Hits, up to {tools} calls in one turn when they do not depend on each other. A {size} answer needs {pages} fetched pages.
4. Check: read what came back. Search or fetch again only to fill a gap you can name, such as a detail the fetched pages do not cover or a claim that only one page makes.
5. Answer: write the final answer as plain text with no tool call. That reply ends the run.
</workflow>
",
        tools = budget.max_tools_per_turn.max(1),
        size = size.as_str(),
        pages = pages_target(size),
    )
}

fn budget_rules(budget: &LoopBudget, reads_pages: bool) -> String {
    let mut text = format!(
        "<budget>
- This run has {turns} turns (model replies), counting the reply that writes the final answer. After each tool turn, the newest tool result says how many turns are left. A reply that still calls a tool on the last turn ends the run with no answer at all, so answer by the last turn at the latest.
",
        turns = budget.max_turns,
    );
    if reads_pages {
        text.push_str(&format!(
            "- A failed fetch is not Evidence: fetch a different Hit rather than retrying the same URL.
- If search is blocked, or still finds nothing useful after one reworded retry, answer from the pages you already have and say what you could not verify.
- Long pages are cut after {chars} characters and end with `[truncated N chars]`. If the detail you need was cut, answer from what you have and say so: fetching copies of the same page from other sites wastes turns.
",
            chars = budget.max_evidence_chars,
        ));
    }
    text.push_str("</budget>\n");
    text
}

/// One worked answer, shown only to runs that can cite fetched pages. Its
/// topic differs from any goal on purpose: it demonstrates the parsed shape
/// (summary first, `## ` themes, `- ` bullets, inline links), not content.
const ANSWER_EXAMPLE: &str = "<example>
The shape of a final reply (topic and URLs are only an illustration; the size above sets how many themes and bullets to write):
`serde` is a framework for serializing and deserializing Rust data structures; each data format, such as JSON, lives in its own crate ([Overview · Serde](https://serde.rs/)).

## Deriving the traits
- `#[derive(Serialize, Deserialize)]` generates both implementations at compile time ([Using derive · Serde](https://serde.rs/derive.html)).
- The derive macros need the `derive` feature of the `serde` crate ([Using derive · Serde](https://serde.rs/derive.html)).
</example>
";

fn answer_format(size: SynthesisSize, reads_pages: bool) -> String {
    let mut text = String::from(
        "<answer_format>
Your final reply goes to the caller verbatim, so it holds only the answer: no remark about your research before or after it. When you are ready to answer, do not announce it; start the reply with the summary. A parser splits the reply into a summary, themes, bullet points and citations, and drops anything outside that shape:
- The summary is the text before the first heading. Its first words already answer the goal: a remark such as \"I have enough information\" or \"Based on the fetched pages\" would become the summary the caller reads.
- Then one `## ` heading per theme, each followed by `- ` bullets. Make every bullet a self-contained point: in a section that has bullets, lines that are not bullets are dropped. Use no other heading levels.
- Code blocks do not survive the parser: put short code inline in backticks inside a bullet.
",
    );
    if reads_pages {
        text.push_str("- Cite with inline markdown links `[page title](url)` in the summary and in the bullets they support, using the URL on each fetched page's `Source:` line. Every theme should cite at least one fetched page.\n");
    }
    text.push_str(&format!(
        "- Write in the language of the research goal, even when the sources are in another language.
- Size for this run: {size}, which means {shape}.
</answer_format>
",
        size = size.as_str(),
        shape = size_shape(size),
    ));
    if reads_pages {
        text.push('\n');
        text.push_str(ANSWER_EXAMPLE);
    }
    text
}

/// Build the system prompt for one run: role, the offered tools with their
/// one-line purposes, evidence rules, workflow, turn budget, and the answer
/// contract `answer::parse_answer` enforces. Sections that direct a tool
/// appear only when that tool is offered.
pub fn build_system_prompt(
    tool_roster: &[(&str, &str)],
    size: SynthesisSize,
    budget: &LoopBudget,
) -> String {
    let offered = |name: &str| tool_roster.iter().any(|(tool, _)| *tool == name);
    let reads_pages = offered(FETCH_TOOL_NAME);
    let mut prompt = format!("{ROLE}\n\n<tools>\n");
    if tool_roster.is_empty() {
        prompt.push_str("None: answer without tools.\n");
    }
    for (name, purpose) in tool_roster {
        prompt.push_str(&format!("- {name}: {purpose}\n"));
    }
    prompt.push_str("</tools>\n\n");
    prompt.push_str(if reads_pages {
        EVIDENCE_RULES
    } else {
        UNREAD_RULES
    });
    if reads_pages && offered(SEARCH_TOOL_NAME) {
        prompt.push_str(&workflow(size, budget));
    }
    if !tool_roster.is_empty() {
        prompt.push_str(&budget_rules(budget, reads_pages));
    }
    prompt.push_str(&answer_format(size, reads_pages));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model told to call a tool the run does not offer burns turns on
    /// failed calls until the repair budget aborts the run: every tool the
    /// prompt directs by name must be one the run offers.
    #[test]
    fn prompt_never_directs_a_tool_that_is_not_offered() {
        let budget = LoopBudget::default();
        let search_only = build_system_prompt(
            &[(SEARCH_TOOL_NAME, "find pages")],
            SynthesisSize::Medium,
            &budget,
        );
        assert!(!search_only.contains("`fetch`"), "{search_only}");
        let none = build_system_prompt(&[], SynthesisSize::Small, &budget);
        assert!(
            !none.contains("`fetch`") && !none.contains("`search`"),
            "{none}"
        );
        let full = build_system_prompt(
            &[(SEARCH_TOOL_NAME, "find pages"), (FETCH_TOOL_NAME, "read one")],
            SynthesisSize::Medium,
            &budget,
        );
        assert!(
            full.contains("`fetch`") && full.contains("`search`"),
            "{full}"
        );
    }
}
