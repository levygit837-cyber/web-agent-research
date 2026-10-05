# Harness contract: calling Web Search

A Harness calls Web Search as one shell command and reads one JSON object from stdout. The calling model sees only the Synthesis, never raw pages.

## Command

```sh
web-agent-research research "<goal>" --json [--size small|medium|large] [--max-turns N]
```

- `--size` (default `medium`): answer length.
- `--max-turns` (default `8`): total LLM calls allowed; a run that reaches this without an answer exits `5`. The agent is told this budget up front and, after each tool turn or empty-reply repair, how many turns are left (#72).
- `--session-id`: optional, default `<unix-secs>-<pid>`. It becomes the file name and the `prompt_cache_key`, so it MUST match `[A-Za-z0-9._-]{1,64}` with no leading dot; anything else exits `2` before any LLM call.
- `--session-out`: optional. By default the Session is written to `<data_root>/sessions/<session-id>.jsonl` (see "Data root and Sessions"), never into the current directory. An explicit path is your contract: if it cannot be written the run exits `6`.

## Environment

| Variable | Required | Default | Invalid value |
|---|---|---|---|
| `GATEWAY_API_KEY` | yes | none | empty/missing exits `2` (`NotConfigured`) |
| `GATEWAY_API_FORMAT` | no | `openai` | `openai` (Chat Completions, `POST {base}/chat/completions`, Bearer auth) or `anthropic` (Messages, `POST {base}/messages`, `x-api-key` + `anthropic-version: 2023-06-01`), case-insensitive; anything else exits `2` (`NotConfigured`) |
| `GATEWAY_BASE_URL` | no | `http://localhost:8317/v1` (the endpoint path is appended per `GATEWAY_API_FORMAT`) | n/a (any non-empty string accepted) |
| `GATEWAY_MODEL` | no | `muse-spark-1.3` | n/a (any non-empty string accepted) |
| `GATEWAY_REASONING_EFFORT` | no | unset (nothing sent) | must be one of `none\|minimal\|low\|medium\|high\|xhigh\|max`; anything else exits `2` (`NotConfigured`). OpenAI format: top-level `reasoning_effort`. Anthropic format: `output_config.effort`, and `none`/`minimal` exit `2` (no Messages equivalent) |
| `GATEWAY_MAX_TOKENS` | no | OpenAI format: unset (no `max_completion_tokens` cap sent). Anthropic format: `8192` plus any enabled thinking budget (`max_tokens` is required there) | not a valid `u32` exits `2` (`NotConfigured`) |
| `GATEWAY_THINKING_BUDGET` | no | unset (no `thinking` sent) | not a valid `u32` exits `2`; `0` sends `thinking: {"type": "disabled"}`; `> 0` sends `{"type": "enabled", "budget_tokens": n}`; if `GATEWAY_MAX_TOKENS` is also set and is `<=` this budget, exits `2` (reasoning tokens count against `max_tokens`). Anthropic format: `1..=1023` exits `2` (API minimum is 1024) |
| `GATEWAY_PROMPT_CACHE_KEY` | no | enabled: every OpenAI-format request carries `prompt_cache_key` set to the run's Session id | `off` disables it (field omitted); any other value is ignored and caching stays enabled. Not sent in the Anthropic format, which caches through `cache_control` breakpoints instead |
| `GATEWAY_EXTRA_BODY` | no | unset (no extra fields merged) | must be a JSON object; invalid JSON or a non-object value exits `2` (`NotConfigured`) |
| `GATEWAY_TIMEOUT_SECS` | no | `60` (per-attempt request timeout) | not a valid integer exits `2` (`NotConfigured`) |
| `GATEWAY_MAX_ATTEMPTS` | no | `3` (total attempts incl. the first try) | not a valid `u32` exits `2` (`NotConfigured`) |
| `GATEWAY_CONTEXT_WINDOW` | no | `200000` when `GATEWAY_MODEL` starts with `claude`, else `128000` (tokens) | not a valid `u32`, or a window that leaves under 16000 characters of context after the reply reserve, exits `2` (`NotConfigured`). Sizes the context budget and the `fetch` part size, see "Page parts and context budget" |

The binary reads only the process environment; it does not load `.env` itself. Locally, keep these in the git-ignored `.env` and export them before a run: `set -a; . ./.env; set +a`.

`GATEWAY_EXTRA_BODY` merges into the wire request body (OpenAI SDK `extra_body` semantics) in both formats: useful for gateway-specific fields this client has no typed support for yet, e.g. `{"reasoning": {"effort": "high"}, "prompt_cache_retention": "24h", "verbosity": "low"}` (OpenAI) or `{"service_tier": "auto"}` (Anthropic). On a key collision, the typed fields this client sends (`model`, `messages`, `system`, `max_tokens`/`max_completion_tokens`, `reasoning_effort`/`output_config`, `thinking`, `prompt_cache_key`, `tools`, `tool_choice`) always win; `GATEWAY_EXTRA_BODY` only fills in keys this client does not otherwise send.

### Anthropic format

- Transcript mapping: system messages become the top-level `system` array; each tool turn becomes one `assistant` message (any `thinking`/`redacted_thinking` blocks first, echoed back verbatim with their `signature`, then text, then `tool_use` blocks) followed by one `user` message holding only the `tool_result` blocks, so every `tool_use` id is answered in the next message.
- Prompt caching: up to three `cache_control: {"type": "ephemeral"}` breakpoints per request (last tool when tools are offered, last system block, last block of the newest message). The prefix order is `tools → system → messages` and the transcript is append-only, so each turn reads what the previous one wrote. Below the model's minimum cacheable prefix (e.g. 4,096 tokens on Claude Haiku 4.5, 1,024 on Sonnet 4.5) nothing is cached and no error is returned.
- Usage: `prompt_tokens` is the full input (`input_tokens + cache_creation_input_tokens + cache_read_input_tokens`), `cached_prompt_tokens` the cache reads, `cache_creation_prompt_tokens` the cache writes.
- `stop_reason` maps to the OpenAI vocabulary internally (`end_turn`/`stop_sequence` → `stop`, `max_tokens`/`model_context_window_exceeded` → `length`, `tool_use` → `tool_calls`); `refusal` fails the turn.
- With thinking enabled the API only accepts `tool_choice` `auto`/`none`; this client always sends `auto`.

### Provider notes

- Anthropic's OpenAI-compatible layer does not support prompt caching (`prompt_cache_key` has no effect there); use `GATEWAY_API_FORMAT=anthropic` against a native Messages endpoint for `cache_control` breakpoints.
- Reasoning/thinking tokens count against `max_completion_tokens`/`max_tokens`, not a separate budget: `GATEWAY_THINKING_BUDGET` must stay strictly below `GATEWAY_MAX_TOKENS` when both are set (some providers, e.g. Anthropic via OpenRouter, enforce this themselves and error otherwise).
- Claude Haiku 4.5 supports manual thinking only (`budget_tokens`), not `output_config.effort`: leave `GATEWAY_REASONING_EFFORT` unset for it.

### Data root and Sessions

The tool keeps two per-user roots, resolved separately. Neither is ever the current directory, so a Harness can run it from any project without creating files there.

| Root | Holds | Resolution (first non-empty wins) |
|---|---|---|
| Data root | Sessions, under `<data_root>/sessions/` | `WEB_AGENT_RESEARCH_HOME`, else `$XDG_DATA_HOME/web-agent-research`, else `$HOME/.local/share/web-agent-research` |
| Cache root | search cache and `engines.json` (disposable), see "Search cache" | `SEARCH_CACHE_DIR`, else `$XDG_CACHE_HOME/web-agent-research`, else `$HOME/.cache/web-agent-research` |

| Variable | Required | Default | Invalid value |
|---|---|---|---|
| `WEB_AGENT_RESEARCH_HOME` | no | unset (falls back to `XDG_DATA_HOME`/`HOME`) | n/a (any non-empty path accepted) |
| `SESSION_MAX_BYTES` | no | `268435456` (256 MiB) of `sessions/*.jsonl`, oldest-first eviction by modification time | not a valid `u64` is ignored, default cap applies |

- Defaulted path is best-effort. If the data root cannot be resolved or written (read-only checkout, sandbox, full disk), the run prints `warning: Session not saved: <reason>` to stderr, skips the rest of the Session, and still exits `0` with the Synthesis. Read `session_id` from the JSON to know the Session name; it exists on disk only if there was no warning.
- An explicit `--session-out` is never best-effort: a write failure exits `6` after the run finished, and nothing is written under the data root.
- The sessions directory is created with mode `0700` and Session files with `0600` on unix (they hold the goal and fetched page bodies).
- Retention: after a defaulted write, the oldest `*.jsonl` files in `sessions/` are deleted until the directory fits `SESSION_MAX_BYTES`. The Session just written is never evicted; explicit `--session-out` files are never counted or deleted.
- To clear Sessions, delete `<data_root>/sessions/`.

### Search cache

`web::search::cache` (#67) persists search-leg results per (engine, normalized query, recency, page) under `<cache_root>/search/` so a repeated `search` call across turns or runs skips the HTTP request entirely. Cache root resolution (`web::cache_dir`): `SEARCH_CACHE_DIR`, else `$XDG_CACHE_HOME/web-agent-research`, else `$HOME/.cache/web-agent-research`; when none of those resolve (both `HOME` and `XDG_CACHE_HOME` unset/empty) caching is silently disabled, never an error. The cache root is separate from the data root that holds Sessions (see "Data root and Sessions").

| Variable | Required | Default | Invalid value |
|---|---|---|---|
| `SEARCH_CACHE_DIR` | no | unset (falls back to `XDG_CACHE_HOME`/`HOME`) | n/a (any non-empty path accepted) |
| `SEARCH_CACHE` | no | enabled | `off` disables both lookup and store; any other value is ignored and caching stays enabled |
| `SEARCH_CACHE_TTL_SECS` | no | `86400` (24h), `3600` (1h) when the `search` call's `recency` is `day` | not a valid `u64` is ignored, default TTL applies |
| `SEARCH_CACHE_MAX_BYTES` | no | `20971520` (20 MiB) of `search/`, oldest-first eviction by fetched-at | not a valid `u64` is ignored, default cap applies |

To clear the cache, delete the resolved cache root's `search/` directory (`rm -rf "${SEARCH_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/web-agent-research}/search"`), or set `SEARCH_CACHE_DIR` to an empty/fresh directory for one run.

### Engine governor

`web::search::governor::Governor` (#62, #63, #64, #66) skips walled engines, paces requests like a person, and keeps Startpage off by default. State (suspension + last-request time) persists to `<cache_root>/engines.json` (same root as the search cache above); a corrupt or missing file loads as empty, never an error.

| Variable | Required | Default | Invalid value |
|---|---|---|---|
| `SEARCH_ENGINES` | no | `duckduckgo,brave,yahoo,bing` (Startpage stays opt-in only, #64: it serves an Anubis proof-of-work challenge to bot-detected clients, and soliciting it would deliberately bypass that wall) | comma list, case-insensitive `duckduckgo`/`ddg`, `startpage`/`sp`, `brave`, `yahoo`, `bing`; unknown tokens are ignored; a result that ends up empty after filtering falls back to the default |
| `SEARCH_BING_MARKET` | no | `en-US` (`mkt` sent on every Bing request; `setlang` is always derived from its language subtag, e.g. `pt-BR` → `setlang=pt`. #66: Bing silently geo/language-localizes on client IP with zero explicit signal otherwise — measured 0/10 relevant results from a non-US IP with no `mkt`, 5/5 after adding it) | an empty/whitespace-only override falls back to the default; any other non-empty string is sent verbatim as `mkt` |
| `SEARCH_SUSPEND_CHALLENGE_SECS` | no | `3600` | not a valid `u64` falls back to the default |
| `SEARCH_SUSPEND_403_SECS` | no | `180` | not a valid `u64` falls back to the default |
| `SEARCH_SUSPEND_429_SECS` | no | `180` (a response's `Retry-After` header, when present, wins over this default) | not a valid `u64` falls back to the default |
| `SEARCH_SUSPEND_UPSTREAM_SECS` | no | `30` (other non-2xx/non-403/429 responses; `0` disables suspension for this kind) | not a valid `u64` falls back to the default |
| `SEARCH_SUSPEND_TIMEOUT_SECS` | no | `0` (a fan-out `Timeout` never suspends by default; a slow leg is as likely to be the local network or the fan-out deadline as a bot wall) | not a valid `u64` falls back to the default |
| `SEARCH_PACE_MIN_MS` / `SEARCH_PACE_MAX_MS` | no | global override, no per-engine default when set (see per-engine defaults below) | not a valid `u64` falls back to the default; a max at or below min degenerates to that fixed gap |
| `SEARCH_PACE_<ENGINE>_MIN_MS` / `_MAX_MS` (`<ENGINE>` = `DUCKDUCKGO`/`STARTPAGE`/`BRAVE`/`YAHOO`/`BING`) | no | falls back to `SEARCH_PACE_MIN_MS`/`_MAX_MS`, else a per-engine default (#73, live-measured 2026-09-29): Bing/Yahoo `1000`/`2000` (cleared an 8s→1s step-down plus a 20+ request burst at 1s with zero blocks); Brave/DuckDuckGo/Startpage `1500`/`4000` (neither reached a genuine gap-tolerance measurement) | same as above |
| `SEARCH_CONCURRENCY` | no | global override, no per-engine default when set | not a valid `u64` falls back to the default |
| `SEARCH_CONCURRENCY_<ENGINE>` | no | falls back to `SEARCH_CONCURRENCY`, else a per-engine default (#73): Bing/Yahoo `2` (measured: zero blocks on a live 2-in-flight probe); Brave/DuckDuckGo/Startpage `1` (Brave: zero measured burst tolerance; DuckDuckGo: consistent with its documented suspension history on this IP) | same as above; read once at `Governor::new` construction, not re-read per call |
| `SEARCH_MAX_REQUESTS_PER_ENGINE` | no | global override, no per-engine default when set | not a valid `u64`/`u32` falls back to the default |
| `SEARCH_MAX_REQUESTS_PER_ENGINE_<ENGINE>` | no | falls back to `SEARCH_MAX_REQUESTS_PER_ENGINE`, else a per-engine default (#73): Bing/Yahoo/Startpage `200`; Brave/DuckDuckGo `10` (circuit breaker -- neither has a measured burst tolerance above the block that ended its #73 session, on top of, not instead of, the existing suspension that already skips a Challenged engine outright) | further requests beyond the cap degrade to `Throttled`, not a bot-wall signal |
| `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL` | no | `4` (cap on Queries dispatched to one engine within a single call; see the pacing note below) | not a valid `u64` falls back to the default |
| `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL_<ENGINE>` | no | falls back to `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL`; no #73 measurement argued for a different value per engine | same as above |
| `SEARCH_MAX_PAGES` | no | global override, no per-engine default when set | not a valid `u64` falls back to the default; always clamped to at least 1 |
| `SEARCH_MAX_PAGES_<ENGINE>` | no | falls back to `SEARCH_MAX_PAGES`, else a per-engine default (#73, live-measured 2026-09-29): Yahoo `3` (genuinely paginates via `b`/`pz`, 0% cross-page URL overlap measured, no sign of exhaustion at depth 5); DuckDuckGo `5` (safety ceiling on its pre-existing `s`/`vqd` continuation, not a behavior change); Bing/Brave/Startpage `1` (Bing's `first=` confirmed dead without JS; Brave's `offset=` unmeasured -- blocked on request #1 before reaching it; neither ships pagination code, so this default has no effect for them) | same as above; read fresh per leg, not cached |

Engine priority for consensus merge and leg dispatch order (`SearchProvider::priority`, from the #65 evaluation's measured relevance/breadth data): Startpage, Brave, Yahoo, DuckDuckGo, Bing (lowest value first).

Suspension: a `Challenge`, HTTP `403`/`429`, other `Upstream` (5xx/transport), or `Retry-After` leg error suspends that engine for the mapped duration (idea inspired by SearXNG's `search.suspended_times`, AGPL-3.0, no code reuse); a suspended engine is skipped with zero HTTP requests for the rest of the suspension. A later, shorter suspension never overwrites a longer one still active. Only a suspension recorded from a live `Challenge` counts as a bot-wall signal for the check below -- a suspension from a 5xx/transport/429/403/`Retry-After` failure does not. If every enabled engine is Challenge-suspended (or live-Challenged), the fan-out fails with a typed error naming the engines and the time left, which reaches the same exit `7` `SearchBlocked` path below …

Pacing and concurrency (#73): each engine has its own concurrency limit (`SEARCH_CONCURRENCY_<ENGINE>` above), enforced by a per-engine semaphore that a leg holds for its whole run -- however many wire requests that leg turns out to need (DDG's continuation re-POSTs, Yahoo's paginated pages) -- so different legs to the *same* engine may now genuinely overlap up to that limit, while different engines always ran in parallel with each other. Right after acquiring a permit, the gate rechecks suspension and the per-run budget -- closing the race where a sibling leg for the same engine settled while this leg was still queued -- before waiting the jittered gap since the last leg *dispatched* to this engine (not the last wire request: a multi-request leg's own internal follow-up requests are not independently gapped or re-checked) and dispatching. `SOFT_DEADLINE_SECS`/`HARD_DEADLINE_SECS` (5 s / 30 s, unchanged) still bound one fan-out call: the default `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL` of 4 is sized so the 4th Query to one engine waits at most `3 * SEARCH_PACE_MAX_MS` before its request even starts, leaving comfortable headroom inside the 30 s hard deadline for the round-trips themselves.

Pagination (#73): Yahoo and DuckDuckGo paginate past page 1, internally, up to `SEARCH_MAX_PAGES_<ENGINE>`'s depth, before returning to the fan-out spawn loop -- so pacing/suspension gate only each leg's first wire request (the same as any other multi-request leg, e.g. DDG's pre-existing continuation), not each individual page. The cache (`cache::lookup`/`cache::store`, always called with `page: 0`) holds one entry per (provider, query, recency) for the whole leg's already-merged rows across every page it fetched, not one entry per page. Yahoo stops early on a page shorter than its expected size (10 rows page 1, 7 rows after -- the only end-of-results signal in its markup); DuckDuckGo stops early when its continuation form is missing or its `s` token stops advancing. Bing and Brave stay page-1-only.

Hermetic rule: `Searcher::with_bases` and every test use a `Governor` that never persists, never paces, has no cap, always allows concurrency 1 per engine, and always enables every engine, regardless of ambient `SEARCH_*` env vars -- the suite never touches the real cache or the network's pacing state.

Recency (#81): when the `search` tool call's `recency` is set, a leg runs only if its engine applies that window (`SearchProvider::applies_recency`, live-verified 2026-10-02, `docs/research/search-engines.md` "Recency windows (#81)") -- the rest are skipped outright: no request, no `errors` entry, no suspension, no cache lookup or write. DuckDuckGo (`df`) and Startpage (`with_date`) apply every window (`day`/`week`/`month`/`year`); Yahoo (`btf`) applies `day`/`week`/`month` only, no `year`; Brave and Bing apply none (Brave's `tf` is unverified -- the one live probe got HTTP `429` before any filtered-vs-unfiltered comparison was possible; Bing has no recency param to begin with). If `recency` is set and no enabled engine applies that window, the call makes zero HTTP requests and returns a one-line note instead of unfiltered Hits ("recency `<window>` is not supported by the enabled search engines; search again without recency").

## Fetch markdown cleanup (#79)

`web::fetch::clean::clean_markdown` runs on the result of both fetch paths (the static `reqwest`/`htmd` conversion and the Obscura browser dump) before it is wrapped in `Evidence`, so the agent and the Harness only ever see the cleaned markdown:

- A data URI (`data:<mime>;base64,…` or `data:<mime>,…`) used as an image or link target is replaced by a short marker (`data:<mime> omitted, <size>`); the payload never reaches Evidence.
- An image keeps only its alt text; an image with no alt text is dropped entirely (the agent never fetches images, so the URL is never worth keeping).
- A link's target keeps its path and non-tracking query params but drops known tracking params (`utm_*`, `fbclid`, `gclid`, `ref_src`, `ref_url`, `mc_cid`, `mc_eid`, `igshid`, `icid`, `cmpid`); a link whose visible text ends up empty (directly, or because its only content was an alt-less image) is dropped entirely.
- A run of 200+ printable-ASCII characters without whitespace (long base64/hex blob, minified JSON/JS leaking into text, …) outside a link/image target or code span collapses to `[long token omitted, N chars]`. Non-ASCII text, such as unspaced CJK prose, is never collapsed.
- A run of 2+ blank lines collapses to one.
- Fenced code blocks and table rows are never altered: only the markup around prose, not prose/code/tables themselves. A fence closes only on a line of the same marker with at least the opening length, so a longer fence can carry a shorter one byte-identical.

## Page parts and context budget (#80)

A fetched page is never cut. The registry keeps the whole cleaned markdown for the run and delivers it in parts; the agent asks for the rest with `fetch` and `part`.

- `fetch` takes an optional `part` (integer `>= 1`, default 1). The result starts `Source: <final url>`, then `Part k of N` when the page has more than one part, then the part text; a part that is not the last ends with `[part k of N; call fetch with the same url and part=k+1 for the rest]`.
- A part is at most `max_part_chars` characters. A cut prefers the last line break in the final 10% of the window, else it falls at exactly `max_part_chars`; the parts concatenate to the full page.
- The first `fetch` of a URL makes the one network request and stores the page. Later calls with `part` are served from memory, with no request, even after the first delivery was dropped from the transcript. A repeat `fetch` without `part` returns the `ALREADY FETCHED` pointer with the part count, once a part of the page has reached the model.
- `part` past the end is a dispatch failure that names the page's part count (`part 4 is past the end: the page has 3 parts`); a `part` that is `0` or not an integer is a dispatch failure too. A fresh fetch asking past the end still stores the page, so the retry costs no request, but it delivers nothing: the retry (with or without `part`) is the page's first delivery.
- The Session holds the whole page once per URL (the row of the first call that delivered a part of it), not the delivered part. Continuation parts add no row, and two requested URLs that redirect to one final URL share one stored page and one row. `evidence_urls` lists each fetched URL once.
- Search Hits, notes and failures are still capped at `max_part_chars` with a `[truncated N chars]` marker.

The context budget comes from the model's window, not a fixed number: `max_context_chars = min((window_tokens - reply_reserve_tokens) * 3, 400000)` and `max_part_chars = min(24000, max_context_chars / 8)`.

- `window_tokens` is `GATEWAY_CONTEXT_WINDOW`, else 200000 when `GATEWAY_MODEL` starts with `claude`, else 128000.
- `reply_reserve_tokens` is `GATEWAY_MAX_TOKENS` if set, else 8192, plus `GATEWAY_THINKING_BUDGET` if set.
- A window that leaves less than 16000 characters (`MIN_CONTEXT_CHARS`) after the reserve exits `2` (`NotConfigured`) and names `GATEWAY_CONTEXT_WINDOW`, instead of running with parts too small to read a page.
- 3 characters per token is a deliberately low estimate: prose averages about 4, but markdown full of URLs and identifiers tokenizes worse, and overshooting the window fails the run while undershooting only drops old turns sooner.
- The 400000-character ceiling keeps cost and latency bounded, because the whole transcript is resent every turn. `--size` does not scale the budget: the window belongs to the model, not to the answer length.
- Turn history still drops the oldest whole turns once the transcript passes `max_context_chars`; the registry still serves their pages from memory.

### Measured defaults (#80)

Seven goals, one per #72 category, run with `claude-haiku-4.5` via kiro without thinking, `--json --max-turns 8`, a shared temp `SEARCH_CACHE_DIR` (so both phases saw the same Hits) and a logging proxy on the gateway. Before is `origin/main` at `ecffa46` (pages cut to 4,000 characters), after is this change. The goal wording is reconstructed, because the #72 harness was throwaway: tokio (small), reqwest `ClientBuilder` timeouts (small, the q3 reference page), the E0502 error (medium, q4), latest Rust release notes (medium), iterating a `HashMap` (medium), a pt-BR HTTP server question (medium), async versus sync HTTP clients (large). One run per cell, so turns and latency are noisy; all 14 runs exited 0.

| Goal | Turns before / after | Latency s before / after | Last request KB before / after | Pages fetched before / after |
|---|---|---|---|---|
| tokio | 3 / 3 | 9.9 / 10.9 | 20.6 / 35.6 | 2 / 2 |
| reqwest timeouts (q3) | 8 / 3 | 31.3 / 12.5 | 26.2 / 38.5 | 3 / 1 |
| E0502 (q4) | 4 / 3 | 32.8 / 18.4 | 27.7 / 54.1 | 4 / 3 |
| Rust release notes | 4 / 3 | 21.4 / 15.4 | 22.2 / 43.2 | 2 / 2 |
| `HashMap` iteration | 3 / 3 | 17.9 / 19.7 | 26.3 / 76.7 | 3 / 3 |
| pt-BR HTTP server | 3 / 3 | 16.7 / 22.0 | 25.8 / 69.6 | 3 / 3 |
| async vs sync | 4 / 4 | 33.4 / 38.3 | 27.4 / 71.7 | 4 / 5 |

- The last request is the whole transcript the model saw on its final call, system prompt and tool definitions included. It grew 1.5x to 2.9x and the largest was 76.7 KB, far under the 400000-character ceiling, so no run dropped a turn.
- q3: before, the model fetched three pages of 4,024 characters each (the `ClientBuilder` page among them) and spent all 8 turns. After, it fetched the docs.rs page once (75,771 characters, 4 parts) and answered in 3 turns, naming `timeout`, `read_timeout` and `connect_timeout` from the page. The three methods sit at offsets 12,842 to 13,782, so they are in part 1 and the run never asked for `part=2`.
- A goal that asks for methods further down reads the rest: "list the `ClientBuilder` methods for proxies and TLS root certificates" fetched the same page, then `part=2` and `part=3`, with one HTTP request for the page and none for the parts (4 turns, 84 KB).
- q4: 4 fetches at 4,024 characters became 3 fetches with longer pages, the same correct explanation of E0502, and one turn fewer.
- The async-vs-sync citation count swung from 15 to 3 in one after run. Two repeats of each phase gave 15, 15, 15 before and 3, 15, 20 after, so it is run-to-run noise in how the model formats links, not an effect of the budget.
- Latency moved both ways, within the spread of repeated runs: async-vs-sync took 28.6 to 35.9 s before and 29.3 to 38.3 s after. More characters per turn costs a little, and fewer turns saves more.

Chosen defaults and why:

- Part size 24,000 characters (`MAX_PART_CHARS`). Across the 19 distinct pages these runs fetched, 14 fit in one part at 24,000, 11 at 16,000 and 17 at 32,000; the whole set is 27, 33 and 23 parts respectively. 16,000 splits common articles (12,000 to 19,000 characters) into two parts and costs a turn each. 32,000 saves only 4 parts across the set but makes every later turn carry up to 8,000 more characters per page. 24,000 is about 8,000 tokens, 4% of a 200K window.
- Context 400,000 characters ceiling (`MAX_CONTEXT_CHARS_CEILING`), reached for a 200K-token window (`(200000 - 8192) * 3 = 575,424`). Eight full parts fit (`PARTS_PER_CONTEXT`), so a run that reads a long page in parts keeps its earlier turns. The measured runs peaked at 77 KB, so the ceiling costs nothing in normal use and bounds cost and latency when the agent reads many parts.
- `--size` does not scale either number, since the window belongs to the model, not to the answer length.

## Latency

Measured tool numbers, not end-to-end run time:

- Search fan-out: ~1.7 s live for 1 query returning 5 Hits (unpaced; the engine governor's per-engine pacing gap, default 1000..2000 ms for Bing/Yahoo or 1500..4000 ms for the rest (#73), applies on top of this for a second Query to the same engine, and persists across back-to-back CLI runs).
- Static fetch (reqwest + htmd, no browser): 0.25–0.96 s — docs.rs 0.25 s, react.dev 0.41 s, wikipedia 0.63 s, github.com 0.96 s.
- Browser fallback (Obscura, JS shells/challenge pages): 1.5–8.2 s.

End-to-end run time is dominated by LLM turns, bounded by `--max-turns`, not by search/fetch. Measured live (#33, `docs/e2e-evidence.md`): 7.7 s for a 2-turn run on `claude-haiku-4.5` that fetched 2 pages.

## Browser profile bump (#59)

`src/web/profile.rs` pins Chrome stable and its two previous majors (`STABLE_MAJORS`), across macOS/Windows/Linux, so `web/search`'s DDG, Yahoo, and Startpage legs, and `web/fetch`'s static fetch path, never send a UA family/version that DDG, Startpage, or a fetched host can flag as stale. One profile is drawn at random per request chain (one engine leg plus its follow-ups for DDG/Yahoo/Startpage, one per fetch call for the static fetch path: DDG's `s`/`vqd` pagination re-POSTs, Yahoo's `b`/`pz` paginated pages (#73), and the Startpage homepage → search POST all reuse the same profile -- DDG's `vqd` token is bound to the UA, and every multi-request leg follows the same one-profile-per-chain rule regardless).

To bump when Chrome ships a new stable major:

1. Check `https://versionhistory.googleapis.com/v1/chrome/platforms/{mac,win,linux}/channels/stable/versions?pageSize=3` for the current stable major per platform.
2. Update `STABLE_MAJORS` in `src/web/profile.rs` to the new stable major and its 2 predecessors.
3. Run `cargo test -- --ignored profile_table_matches_chrome_stable` (a live network call) to confirm the table now matches. This is a manual step: no CI job runs `--ignored`/`--include-ignored`, so a lagging table is caught only by running this command, not automatically.
4. `cargo test web::profile::` for the coherence suite (`every_profile_is_internally_coherent`): every profile's UA major must equal its `sec_ch_ua` version, and its platform must equal `sec_ch_ua_platform`.

Only Chrome-family profiles exist while the transport is plain `reqwest`/rustls (no TLS/JA3 emulation, #56/#60): a Firefox or Safari UA over a generic rustls ClientHello would be its own family/TLS mismatch signal.

## Output (`--json`, exit 0)

```json
{
  "session_id": "1790000000-4242",
  "synthesis": {
    "size": "medium",
    "summary": "…",
    "themes": [{ "title": "…", "points": ["…"], "citations": [{ "url": "https://…", "title": "…" }] }],
    "citations": [{ "url": "https://…", "title": "…" }]
  },
  "turns_used": 4,
  "evidence_urls": ["https://…"],
  "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0, "reasoning_tokens": 0, "cached_prompt_tokens": 0, "cache_creation_prompt_tokens": 0 }
}
```

- `evidence_urls`: pages actually fetched in this run. Hits that were never fetched are not included.
- `citations` (top-level and per theme): only links to pages fetched in this run, matched against each page's final (post-redirect) URL after normalization (`www.` and trailing-slash variants count as the same page). Links the model copied from inside a fetched page, or to Hits it never fetched, are dropped from `citations`; the prose in `summary`/`points` is left as written.
- `synthesis` comes from the agent's final reply (its first reply with no tool call). The agent is asked to put its answer inside an `<answer>` … `</answer>` block, each tag on a line of its own, and only the text inside the last such block is parsed, so remarks about its own progress never reach `summary` (#72). A tag counts only when it is alone on its line, in any letter case, so a tag mentioned in a remark or shown in inline code stays text. A reply cut before its closing line parses from the opening line to the end; a reply with no opening line is parsed whole. `summary` is the text before the first `## ` heading, each `## ` section is one theme, and its `- `/`* `/`N. ` bullets are its `points`.
- The agent is told to fetch pages before answering and to cite only pages it fetched, but the loop enforces neither. In the #72 measurement every run after the prompt change fetched at least 2 pages (`docs/research/agent-prompt.md`), so no guard was added. An exit-0 run with empty `evidence_urls` read no page, so treat its Synthesis as unverified. A citation means the page was fetched, not that every claim beside it is in the part the agent read: a long page reaches the agent in parts of up to 24,000 characters (see "Page parts and context budget"), and the agent chooses which parts to read.
- Without `--json`, stdout is human-readable markdown: summary, `##` themes, and a numbered `Sources:` list.

## Exit codes

| Code | Meaning | Harness action |
|---|---|---|
| 0 | Synthesis on stdout | use it |
| 2 | Not configured: empty goal, missing key, `max_turns < 1`, an invalid `--session-id`, an invalid `GATEWAY_*` value, or a `GATEWAY_CONTEXT_WINDOW` too small to leave 16000 characters of context | fix env/args; do not retry |
| 3 | Gateway exhausted: retries/rate limit, auth failed (`bad API key`, HTTP 401), or access refused (`refused access (HTTP 403): <upstream message>`) | 401: fix key; 403: the key works but the model/tier is gated upstream, pick another model; otherwise retry later |
| 4 | Tool failures exceeded the repair budget | retry, or rephrase the goal |
| 5 | Turn budget exhausted | retry with a higher `--max-turns` |
| 6 | Session I/O failed on an explicit `--session-out` (a defaulted Session path never exits `6`: it warns on stderr and exits `0`) | check the `--session-out` path, or omit it |
| 7 | Search blocked: the run finalized with no fetched Evidence, no `search` call ever returned a Hit, and every leg of at least one `search` call was either a bot-detection Challenge (DuckDuckGo anomaly page, Startpage Anubis proof-of-work/CAPTCHA, Brave or Bing HTTP `429`, Bing `challenge/verify` gate, Yahoo consent redirect) or a skip of an engine already suspended *from* such a Challenge (#62). A `search` call narrowed by `recency` never counts, since the same search without `recency` could still reach the skipped engines (#81) — the search engines are walled from this network, not that nothing exists | retry later or from a different network/IP; do not treat as "no results" |

Errors are printed to stderr as `error: <message>`; stdout stays empty.

## Harness snippet

Add this to the Harness's agent instructions (omp `AGENTS.md`, Claude Code `CLAUDE.md`, or similar):

```md
For questions that need current web information, run
`web-agent-research research "<question>" --json --size small` with the shell tool
and answer from `synthesis`; cite `synthesis.citations[].url`. Exit code != 0 means
no answer; stderr says why.
```
