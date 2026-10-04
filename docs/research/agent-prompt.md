# Agent prompt evaluation (#72)

The Web Search agent sometimes answered from search snippets without fetching a single page, so the Harness got a Synthesis with `evidence_urls: []` and links that `citations` then dropped. This note records what the model receives each turn, the gaps found in that text, what changed, and the before/after measurement. File and line references in the gap table point at the pre-fix commit `4d3ef27`.

## What the model receives

- System prompt: `research::prompt::build_system_prompt`, built once per run from the offered tools, the size and the `LoopBudget`, so it is byte-identical across turns and the prompt-cache prefix holds. Sections: role, `<tools>`, `<evidence_rules>`, `<workflow>`, `<budget>`, `<answer_format>`, `<example>`. A section that directs a tool only appears when that tool is offered.
- Tools: `search` (`src/web/search/tool.rs`) and `fetch` (`src/web/fetch/tool.rs`), each a name, a description and a JSON Schema. The system prompt only lists them with one-line purposes, because the API already sends the definitions.
- First user message: the goal and the requested size (`goal_message` in `src/research/agent_loop/context.rs`).
- After each tool turn: every result rendered by `ToolResult::render` (`src/research/agent_loop/result.rs`). A `fetch` result is one part of the page (#80, up to `max_part_chars`, 24,000 by default); every other result is cut at `max_part_chars`. The newest result ends with the turns left, counting the final answer (`turns_left_note` in `src/research/agent_loop/dispatch.rs`). Per-turn state rides here, in the transcript tail, never in the system prompt.
- The end: the first reply with no tool call and a non-empty answer is the final answer; an empty one gets a repair turn that carries the same turns-left note. `parse_answer` (`src/research/agent_loop/answer.rs`) parses only its last `<answer>` … `</answer>` block, whose tags count only alone on their lines, or the whole reply when it has no opening line.
- Wire: `tool_choice` is always `auto`; Anthropic cache breakpoints sit on the last tool, the last system block and the last message (`src/llm/anthropic.rs`).

## Gaps found

| Gap | Status | Evidence (pre-fix) | Change |
|---|---|---|---|
| G1. The system prompt never says a snippet is not Evidence, and Hits rendered as `- [title](url)`, the citation syntax (`registry.rs:91`) | confirmed | q1 answered after 1 search and 0 fetches: "Based on the search results, here's what tokio is: … [Tokio](https://tokio.rs/)" | `<evidence_rules>`; Hits render as numbered candidates under "Candidates only, not Evidence" |
| G2. "Answer directly … when the evidence suffices" (`prompt.rs:32-38`, repeated in the repair text at `runner.rs:181`) | confirmed | q1: 2 turns, 0 fetches, exit 0 | "Fetch before you answer", with the reason; the empty-answer repair text dropped the phrase |
| G3. Citations neither restricted to fetched pages nor required (`prompt.rs:30`) | confirmed | 3 of 13 links dropped, all Hits never fetched; q4 fetched 3 pages and cited none | Cite only fetched pages, using each page's `Source:` URL; every theme cites one |
| G4. No source-selection guidance | confirmed | q3 fetched two stale third-party mirrors of the reqwest docs | Workflow step "Choose": first-hand sources first, skip mirrors and off-topic Hits |
| G5. No pages-per-size target | confirmed | 0 to 5 fetches per run | small 1-2, medium 2-4, large and deep 3-6 pages |
| G6. The turn budget appears nowhere | confirmed | q3 spent all 8 turns and exited 5 with no answer | `<budget>` section plus the turns-left note after every tool turn |
| G7. Failures render as a bare `FAILED: …` (`registry.rs:101-104`) | confirmed in the text; not exercised | none | Failed fetch, blocked search and no-Hits results say what to do next |
| G8. No rule for the answer's language | confirmed in the text; no failure seen | q7 answered in pt-BR anyway | "Write in the language of the research goal" |
| G9. The answer shape `parse_answer` reads is never spelled out (`prompt.rs:31`) | confirmed | 6 of 6 summaries broken: a progress remark ("I have enough evidence to synthesize a comprehensive answer. Here's the final synthesis:") or a heading pulled into the summary | Explicit shape, one worked example, and the `<answer>` block |
| G10. `Source:` came after the page body, so the cap cut it (`registry.rs:96`) | new | 17 of 18 fetch results lost their source URL | `Source:` is the first line |
| G11. The 4,000-char cap keeps only the top of a long page | fixed in #80 | q3's docs.rs `ClientBuilder` page lost 81,842 chars, including every `timeout` method | Pages come in parts of up to 24,000 chars and the agent reads the rest with `fetch` and `part`; measured in `docs/harness.md` "Measured defaults (#80)" |
| G12. The search description names only DuckDuckGo and Startpage and says recency is "mapped per provider" (`tool.rs:22`, `:42`) | new; recency open | Brave, Yahoo and Bing ignore `recency` (`brave.rs`, `yahoo.rs`, `bing.rs`: `_recency`) | Engine-neutral description; "only some engines apply it" |
| G13. The fetch description is one sentence (`fetch/tool.rs:52`) | new | none | What it returns, the JS fallback, the cut, what `FAILED:` means |

Refuted:

- `tool_choice`: every run called `search` on turn 1, so the skip happens after search and forcing a tool is not the lever. `any` and `tool` also fail with manual extended thinking.
- Prompt caching: the prefix was already stable. Kiro reports no cache token counts, so hits cannot be measured through it.

## Measurement

Seven fixed goals, one per category: crate docs (small), API reference (small), exact error message (medium), recent news (medium), how-to (medium), a pt-BR question (medium), comparison (large). The model is `claude-haiku-4.5` via kiro, without thinking, with the default `LoopBudget`. A local stub serves one recorded Hit list per goal for every query: 5 Yahoo result pages recorded for this evaluation (API reference, error, news, pt-BR, comparison), the #65 fixtures (crate docs) and a #73 Yahoo capture (how-to). No engine saw traffic from the runs, and fetches are real. A logging proxy recorded every request and reply. The harness was throwaway and is not in the repo.

| Phase | Commit | Fetched >= 1 | Exit 0 / 5 | Pages fetched | Citations kept / links | Broken summaries | Turns median, max | Latency s median, max |
|---|---|---|---|---|---|---|---|---|
| before | `4d3ef27` | 6/7 | 6 / 1 | 21 | 10 / 13 | 6/6 | 3, 8 | 16.3, 85.5 |
| after | `142fd66` | 7/7 | 7 / 0 | 19 | 13 / 13 | 3/7 | 3, 6 | 14.8, 23.4 |
| after2 | `f1820db` | 7/7 | 7 / 0 | 21 | 14 / 14 | 3/7 | 3, 7 | 13.4, 24.2 |
| after3 | `9249dc9` | 7/7 | 7 / 0 | 18 | 13 / 13 | 4/7 | 3, 6 | 13.9, 20.9 |
| after4 | `e70d386` | 7/7 | 7 / 0 | 21 | 14 / 14 | 0/7 | 3, 8 | 18.1, 20.9 |

- A broken summary opens with a progress remark or a `## ` heading. Three prompt-wording passes (after to after3) left 3 or 4 of 7 of them. After4 moved the answer into an `<answer>` block: 7 of 7 replies used it, and 2 of them (q6, q7) still wrote a remark outside the block that the parser dropped.
- In the before pass, q4's 85.5 s latency was the gateway: 77.1 s of it were LLM calls.
- The system prompt grew from 1,141 to 4,857 chars.

## Decisions

- The answer block over a remark-stripping heuristic. The remarks were multi-sentence and multilingual ("Excelente! Tenho informações suficientes…"), so a sentence filter would guess. Anthropic's guidance for runs without thinking is to separate reasoning from the final output with `<thinking>` and `<answer>` tags. A reply without the block still parses whole, as before.
- The turns-left note goes in the newest tool result, never in the system prompt, so the cached prefix stays byte-identical.
- No loop guard. The issue allowed one only if wording alone proved unreliable, and 28 of 28 after-runs fetched at least 2 pages. If a model regresses, the natural guard turns a zero-Evidence answer into a repair turn when `fetch` was offered and search returned Hits, counted against `max_repairs`.

## Open

- G12: `recency` has no effect on Brave, Yahoo or Bing.

## Sources

- Anthropic, Prompting best practices: https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/claude-prompting-best-practices. Used: be clear and direct, add context (explain why), XML tags, give Claude a role, tool usage, research and information gathering, control the format of responses, minimizing hallucinations, context awareness, and `<thinking>`/`<answer>` tags when thinking is off.
- Anthropic, Define tools: https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools. Used: detailed descriptions (what, when, parameters, caveats), return only high-signal information, and the tool use system prompt.
