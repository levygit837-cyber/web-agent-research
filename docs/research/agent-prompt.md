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
| G1. The system prompt never says a snippet is not Evidence, and Hits rendered as `- [title](url)`, the citation syntax (`registry.rs:91` before #83; now `result.rs`) | confirmed | q1 answered after 1 search and 0 fetches: "Based on the search results, here's what tokio is: … [Tokio](https://tokio.rs/)" | `<evidence_rules>`; Hits render as numbered candidates under "Candidates only, not Evidence" |
| G2. "Answer directly … when the evidence suffices" (`prompt.rs:32-38`, repeated in the repair text at `runner.rs:181` before #83; now `dispatch.rs`) | confirmed | q1: 2 turns, 0 fetches, exit 0 | "Fetch before you answer", with the reason; the empty-answer repair text dropped the phrase |
| G3. Citations neither restricted to fetched pages nor required (`prompt.rs:30`) | confirmed | 3 of 13 links dropped, all Hits never fetched; q4 fetched 3 pages and cited none | Cite only fetched pages, using each page's `Source:` URL; every theme cites one |
| G4. No source-selection guidance | confirmed | q3 fetched two stale third-party mirrors of the reqwest docs | Workflow step "Choose": first-hand sources first, skip mirrors and off-topic Hits |
| G5. No pages-per-size target | confirmed | 0 to 5 fetches per run | small 1-2, medium 2-4, large and deep 3-6 pages; medium became 3-5 in #124 |
| G6. The turn budget appears nowhere | confirmed | q3 spent all 8 turns and exited 5 with no answer | `<budget>` section plus the turns-left note after every tool turn |
| G7. Failures render as a bare `FAILED: …` (`registry.rs:101-104` before #83; now `result.rs`) | confirmed in the text; not exercised | none | Failed fetch, blocked search and no-Hits results say what to do next |
| G8. No rule for the answer's language | confirmed in the text; no failure seen | q7 answered in pt-BR anyway | "Write in the language of the research goal" |
| G9. The answer shape `parse_answer` reads is never spelled out (`prompt.rs:31`) | confirmed | 6 of 6 summaries broken: a progress remark ("I have enough evidence to synthesize a comprehensive answer. Here's the final synthesis:") or a heading pulled into the summary | Explicit shape, one worked example, and the `<answer>` block |
| G10. `Source:` came after the page body, so the cap cut it (`registry.rs:96` before #83; now `result.rs`) | new | 17 of 18 fetch results lost their source URL | `Source:` is the first line |
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

## Goal coverage (#124)

The 2026-10-07 stage attribution (305 nuggets): 55% of the golden facts were stated, 28% were on a page the agent fetched and left out of the answer, 13% never reached the Hits. Runs used 3.5 turns of 10 and never searched more than twice, so "fill a gap you can name" almost never fired.

Change, all inside the one research loop (no sub-agents):

- Workflow step 4 makes the agent list the goal's explicit sub-questions (each enumeration item, each joined clause, each named command, option, version or comparison) with the fetched page and passage that answers each, or `gap`, outside the answer tags. A `gap` is filled by fetching a skipped Hit or searching again for the missing part.
- Step 5 and an `answer_format` bullet require every sub-question to be covered; one still a `gap` is stated as unverified in the summary, never dropped and never answered from memory. The rule needs `fetch`, so a run without it never sees it.
- The `medium` shape is a 2-4 sentence summary and 3-5 themes with 3-6 bullets each (was 2-3, 2-4, 2-5), and its pages target is 3-5 (was 2-4). A `--size large` replay of the same tapes had raised recall, so the shape moved toward the large one (large is a 3-5 sentence summary and 3-6 themes with 3-6 bullets each).
- The prompt stays byte-stable across turns; `parse_answer` is unchanged.

Measurement, `claude-haiku-4.5` agent, `gpt-5.6-sol` judge, 56 goals:

| Variant | Mode | n | Result vs origin/main |
|---|---|---|---|
| V1: steps 4-5 and the `answer_format` bullet, shape unchanged | `replay`, golden v1 | K=1 | nugget recall +0.034 [-0.022, +0.090]; exact quotes -0.122 [-0.227, -0.017]; correct +0.036 [-0.066, +0.137] |
| V2: V1 and the wider medium shape | `replay`, golden v1 | K=1 | nugget recall +0.067 [+0.014, +0.120]; correct -0.054 [-0.161, +0.054]; claim precision -0.032 [-0.128, +0.064] |
| V2 | `replay-through`, golden v2 | K=3 | table below |

K=3 paired A/B on separate copies of the frozen 2026-10-07 tapes; control is origin/main `adf6d8a`, treatment is V2 on the same base. Both arms were judged with the claim pass.

| Metric | Control | V2 | V2 - control | 95% CI |
|---|---|---|---|---|
| core_recall | 0.874 | 0.884 | +0.010 | [-0.015, +0.035] |
| supporting_recall | 0.429 | 0.467 | +0.038 | [-0.007, +0.084] |
| nugget_recall | 0.673 | 0.694 | +0.021 | [-0.004, +0.046] |
| correct | 0.869 | 0.845 | -0.024 | [-0.085, +0.037] |
| exact_share | 0.702 | 0.671 | -0.031 | [-0.083, +0.020] |
| claim_precision | 0.706 | 0.706 | +0.001 | [-0.044, +0.046] |
| claim_partly | 0.156 | 0.158 | +0.002 | [-0.029, +0.033] |
| claim_unsupported | 0.002 | 0.008 | +0.006 | [-0.001, +0.014] |
| claim_contradicted | 0.010 | 0.012 | +0.001 | [-0.005, +0.008] |
| uncited_share | 0.126 | 0.115 | -0.011 | [-0.054, +0.033] |
| citations | 2.244 | 2.214 | -0.030 | [-0.145, +0.086] |
| turns | 3.506 | 3.607 | +0.101 | [-0.011, +0.214] |
| total_tokens | 848 | 1,070 | +222 | [+179, +265] |
| wall_secs | 15.6 | 18.4 | +2.8 | [+1.1, +4.5] |

Live fetches (`replay_misses` in `replay-through`): control 58 in 44 of 168 runs, V2 54 in 47 of 168.

Reading:

- Every recall and correctness interval contains 0. The direction on recall is positive (nugget +0.021, supporting +0.038) and on correct negative (-0.024), but the K=3 run cannot tell either from noise. The issue's bar, core and nugget recall up with a 95% CI above 0, is not met.
- Claim precision did not move and no claim metric is significantly worse, so the extra coverage did not cost grounding. Exact quotes are down 0.031, inside the noise.
- The only significant differences are cost: +222 completion tokens and +2.8 s per run. The loop still stops at about 3.6 turns, so the sub-question list makes the answer wider but rarely makes the agent search again.
- The K=1 `replay` result of V2 (nugget recall +0.067, CI above 0) did not hold at K=3 on `replay-through` and golden v2. Treat K=1 numbers as a lead, as `docs/eval.md` says.

## Open

- G12: `recency` has no effect on Brave, Yahoo or Bing.

## Sources

- Anthropic, Prompting best practices: https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/claude-prompting-best-practices. Used: be clear and direct, add context (explain why), XML tags, give Claude a role, tool usage, research and information gathering, control the format of responses, minimizing hallucinations, context awareness, and `<thinking>`/`<answer>` tags when thinking is off.
- Anthropic, Define tools: https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools. Used: detailed descriptions (what, when, parameters, caveats), return only high-signal information, and the tool use system prompt.
