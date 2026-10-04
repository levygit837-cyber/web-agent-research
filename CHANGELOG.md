# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `web::fetch::clean::clean_markdown` (#79): a shared markdown post-processing pass, applied to both the static (`reqwest`/`htmd`) and Obscura fetch paths before `Evidence` is built. Data URIs in images/links are replaced by a short `data:<mime> omitted, <size>` marker (the payload never reaches Evidence); images keep only their alt text and are dropped entirely when they have none; link targets keep their path and non-tracking query params but drop known tracking params (`utm_*`, `fbclid`, `gclid`, `ref_src`, `ref_url`, `mc_cid`, `mc_eid`, `igshid`, `icid`, `cmpid`), and a link left with empty visible text is dropped entirely; a non-prose token of 200+ characters outside a link/image target or code span collapses to a short marker; runs of 2+ blank lines collapse to one. Fenced code blocks and table rows are copied through byte-identical. The fetch tool description now states what is stripped.
- `web::search::governor::Governor`: per-engine suspension (#62), human pacing (#63), and an engine allowlist (#64), all persisted (except the allowlist, which is env-only) to `<cache_root>/engines.json`, lazily loaded on first real use so `Searcher::new()` never touches disk. Corrupt or missing state loads as empty, never an error.
  - Suspension: a `Challenge`/`403`/`429`/`Upstream` leg error maps to a suspension duration, idea inspired by SearXNG's `search.suspended_times` (AGPL-3.0, no code reuse — settings-file idea only). Defaults: `Challenge` 3600s, HTTP `403` 180s, HTTP `429` 180s (a real `Retry-After` header wins over the default), other `Upstream` (5xx/unknown) 30s, `Timeout` 0s (no suspension — a slow leg is as likely to be the local network or the fan-out deadline as a bot wall). Each overridable via `SEARCH_SUSPEND_CHALLENGE_SECS`/`SEARCH_SUSPEND_403_SECS`/`SEARCH_SUSPEND_429_SECS`/`SEARCH_SUSPEND_UPSTREAM_SECS`/`SEARCH_SUSPEND_TIMEOUT_SECS`. A suspended engine is skipped with zero HTTP requests, surfacing a typed `SearchProviderError::Suspended`; a success clears a residual suspension. A later, shorter suspension never overwrites a longer one still active (two concurrent legs for the same engine can settle in either order). Overflow-safe: a pathological `Retry-After`/env override that cannot be represented as an `Instant` is dropped rather than panicking.
  - Pacing: a per-engine serial queue, held for a whole leg via `Governor::acquire`/`EnginePermit`, with a random gap since the last leg dispatched to that engine (default 1500..4000ms, `SEARCH_PACE_MIN_MS`/`SEARCH_PACE_MAX_MS`, or per engine `SEARCH_PACE_<ENGINE>_MIN_MS`/`_MAX_MS`); different engines run in parallel with each other. `pace` rechecks suspension and the per-run budget right after acquiring the queue, closing the race where a sibling leg for the same engine settles while this one was still queued. The last-request time per engine persists in `engines.json` so back-to-back CLI runs also respect the gap; a persisted timestamp days in the past correctly imposes zero wait on a fresh process. A per-run request cap per engine (`SEARCH_MAX_REQUESTS_PER_ENGINE`, default 200) and a cap on Queries dispatched to one engine per call (`SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL`, default 4, sized so the worst-case pacing wait for the last capped Query still leaves headroom inside the existing 30s hard fan-out deadline) both degrade a leg to a new `SearchProviderError::Throttled`, never a bot-wall signal.
  - Allowlist: `SEARCH_ENGINES` (comma list, case-insensitive `duckduckgo`/`ddg`/`startpage`/`sp`; default `duckduckgo` only — Startpage is off by default (#64): it serves an Anubis proof-of-work challenge to this residential IP, and soliciting it would deliberately bypass a bot wall). Unknown tokens are ignored; an empty result after filtering falls back to the default.
  - Hermetic rule: `Governor::hermetic()` (used by `Searcher::with_bases` and every existing test) never persists, never paces, has no per-run/per-call cap, and always enables both providers, regardless of ambient `SEARCH_*` env vars.
- `SearchProviderError::Upstream` carries the real HTTP `status` and a parsed `retry_after_secs` (seconds form only); a new `Suspended { provider, remaining_secs, reason }` variant (a live skip) and a new `Throttled { provider, detail }` variant (a local scheduling skip: the per-call query cap or the per-run request cap). `ddg.rs`/`startpage.rs` now surface `status`/`retry_after_secs` from the response.
- `web::search::cache`: on-disk cache for search legs, keyed by normalized query (trim, collapse whitespace, lowercase) + recency + engine + page (#67). Entries live under `<cache_root>/search/`, one file per leg, atomic writes via `cache_dir::write_atomic`. Cached per engine leg, not per merged fan-out output, so a cached leg for one engine and a live leg for another still merge normally. TTL defaults to 24h, 1h when the Query's `recency` is `day`; `SEARCH_CACHE_TTL_SECS` overrides both uniformly; `SEARCH_CACHE=off` disables lookup and store outright. Only a settled `Ok` leg is ever stored: a `Challenge`/`Timeout`/`Upstream` leg error has no code path into the cache. A zero-row result is cached like any other successful leg. Size-capped (`SEARCH_CACHE_MAX_B…
- `web::cache_dir`: shared cache-root resolution (`SEARCH_CACHE_DIR`, else `XDG_CACHE_HOME/web-agent-research`, else `HOME/.cache/web-agent-research`, else disabled) and the atomic-write helper, used by `web::search::governor` (#62/#63) and `web::search::cache` (#67).
- `src/web/profile.rs`: a Chrome-family browser profile table (stable + 2 previous majors, macOS/Windows/Linux) with UA, coherent GREASE `sec-ch-ua`/`sec-ch-ua-mobile`/`sec-ch-ua-platform`, `accept`, `accept-encoding`, `accept-language`; one profile drawn at random per request chain (DDG pagination and Startpage homepage → search POST reuse the same profile, since DDG's `vqd` is bound to the UA) (#59).
- `THIRD-PARTY-NOTICES.md`: attribution and license text for code ported from oh-my-pi (MIT) and Obscura (Apache-2.0); SearXNG credited as prior art only, no code copied (#61).
- `docs/research/search-engines.md`: measured evaluation of 12 candidate free search engines (Brave, Bing, Yahoo, Yandex, Mojeek, Qwant, Marginalia, Wikipedia REST, Google `/wml`, Chatnoir, Mwmbl, Startpage-as-counter-example) against a fixed 10-query set, with keep/maybe/drop verdicts, a priority order for #66, and trimmed raw fixtures under `docs/research/search-engines/fixtures/` (#65). No engine code added; research and measurement only.
- Brave HTML, Yahoo HTML, and Bing HTML search legs (#66), page 1 only (pagination is #73's job): `src/web/search/{brave,yahoo,bing}.rs`.
  - Brave: `search.brave.com/search?q=&source=web` with Brave's own default cookies (`safesearch=off; useLocation=0; summarizer=0; country=us; ui_lang=en-us`); direct unwrapped URLs; HTTP `429` (CloudFront edge, no body marker) maps `Challenge`.
  - Yahoo: `search.yahoo.com/search?p=`, replaying the `YBV` cookie chain across a per-leg cookie jar (a dedicated no-auto-redirect `reqwest::Client`, up to 4 manual hops); `r.search.yahoo.com/.../RU=<url>/RS=` redirect wrapper unwrapped; a redirect to `guce.yahoo.com`/`consent.yahoo.com` maps `Challenge`.
  - Bing: `www.bing.com/search?q=&mkt=&setlang=`, `mkt` always sent (`SEARCH_BING_MARKET`, default `en-US`; `setlang` derived from its language subtag) — Bing silently geo/language-localizes on client IP otherwise, measured 0/10 relevant results from a non-US IP with no `mkt`, 5/5 after adding it; `bing.com/ck/a?...&u=a1<base64url>` redirect wrapper unwrapped; a `challenge/verify` JS marker or HTTP `429` maps `Challenge`.
  - All three send Chrome navigation headers via the existing `apply_navigation_headers`, one browser profile per leg (#59).
  - `SearchProvider` gains `Brave`, `Yahoo`, `Bing` variants; merge/dispatch priority order (from the #65 evaluation's measured relevance/breadth): Startpage, Brave, Yahoo, DuckDuckGo, Bing.
  - Default `SEARCH_ENGINES` becomes `duckduckgo,brave,yahoo,bing`; Startpage stays opt-in only.
  - `Searcher::with_bases`/`search_multi_with_bases` extended with `brave`/`yahoo`/`bing` base-URL parameters (test seam only, `#[doc(hidden)]`).

- `docs/research/search-engines.md`: live per-engine request-budget and pagination measurement for DuckDuckGo, Brave, Yahoo, Bing (#73) — concurrency (2-in-flight tolerance), minimum safe pacing gap, burst budget, cooldown behavior after a block, and pagination-param viability, each dated with status-code evidence. Bing's `first=` pagination confirmed dead without JS; Brave blocked on request #1 this session (unmeasured); DuckDuckGo blocked on request #2 of a single conservative pass; Yahoo's `b`/`pz` pagination confirmed genuinely advancing with 0% cross-page URL overlap.
- `web::search::governor`: per-engine concurrency, pacing gap, and request-cap defaults from the #73 measurement, each independently overridable via env, conservative margin below the measured tolerance (#73):
  - Concurrency: Bing/Yahoo default to 2 in-flight requests (measured: zero blocks on a live 2-in-flight probe); Brave/DuckDuckGo/Startpage stay at 1 (Brave: zero measured burst tolerance; DuckDuckGo: consistent with its documented suspension history on this IP). Implemented via a per-engine `tokio::sync::Semaphore` (replacing the prior always-serial `AsyncMutex`); `SEARCH_CONCURRENCY`/`SEARCH_CONCURRENCY_<ENGINE>` override. A hermetic `Governor` always uses 1 permit per engine regardless of the production default.
  - Pacing gap: Bing/Yahoo tighten to 1000..2000ms (measured: cleared an 8s→1s step-down plus a 20+ request burst at 1s with zero blocks); Brave/DuckDuckGo keep the original 1500..4000ms (neither reached a genuine gap-tolerance measurement this session).
  - Per-run request cap: Brave/DuckDuckGo tighten to 10 (circuit breaker; neither has a measured burst tolerance above the block that ended their session); Bing/Yahoo/Startpage keep 200.
  - Per-call query cap (`SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL`) gains a per-engine override (`_<ENGINE>` suffix); default unchanged at 4 for every engine (no #73 measurement argued for a different value).
- Configurable per-engine pagination depth (`SEARCH_MAX_PAGES`, `SEARCH_MAX_PAGES_<ENGINE>`), read fresh per leg (#73):
  - Yahoo (`src/web/search/yahoo.rs`): `yahoo_search` now paginates via the measured `b`/`pz` param shape (page 1: `iscqry=`, up to 10 rows; page N>1: `b=N*7+1&pz=7&bct=0&xargs=0`, up to 7 rows) up to a default depth of 3. Stops on a short page (fewer rows than that page's expected size — the only end-of-results signal in Yahoo's markup, no explicit next-page marker exists) or at `MAX_NUM_RESULTS`. One client and cookie jar span the whole leg, so page 2+ reuses page 1's warmed `YBV=v0.2` cache cookie instead of repeating the 3-hop redirect chain; every page reuses page 1's browser profile (#59 chain-reuse contract).
  - DuckDuckGo (`src/web/search/ddg.rs`): the existing `s`/`vqd` continuation loop gains a depth cap, default 5 — a safety ceiling against a pathological continuation cycle, not a behavior change for any query this repo has measured paginating (unbounded in practice pre-#73, still bounded by `MAX_NUM_RESULTS` or a missing continuation form).
  - Bing and Brave ship no pagination code: Bing's `first=` pagination is confirmed dead without JS (pages 1–4 byte-identical); Brave's `offset=` pagination is unmeasured this session (blocked on request #1, before reaching it) — the default pagination depth for both is 1 and has no effect.

- `SearchProvider::applies_recency` (#81): per-engine recency capability, live-verified 2026-10-02 (`docs/research/search-engines.md` "Recency windows (#81)"). DuckDuckGo/Startpage apply every window (`day`/`week`/`month`/`year`); Yahoo (`btf`) applies `day`/`week`/`month` only (verified live via the full `YBV` cookie-chain replay: distinct, progressively wider date ranges per window); Brave/Bing apply none (Brave's one live `tf` probe got HTTP `429`, stays unverified rather than guessed; Bing has no recency param). Yahoo's `yahoo_page_query` now wires `&btf=d|w|m` when `recency` is set.
  - `search_multi_with_bases` filters enabled providers down to the ones that apply the requested `recency` window before building legs: an engine that cannot apply the window is skipped outright (no request, no error, no suspension, no cache lookup or write), never run unfiltered. If `recency` is set and no enabled engine applies it, the call makes zero HTTP requests and returns `SearchOutput { note: Some(...) }`, a one-line note telling the model to search again without `recency`, instead of unfiltered Hits.
  - `ToolResult::SearchNote` (`src/research/agent_loop/registry.rs`) carries that note through to the tool-role transcript.
  - The `search` tool schema's description and `recency` field now state the skip rule for the model.
- `fetch` takes an optional `part` (integer `>= 1`) (#80). `part` past the end is a dispatch failure naming the page's part count; `0` or a non-integer is a dispatch failure (`FetchError::InvalidPart`).
- `GATEWAY_CONTEXT_WINDOW` (tokens; default 200000 for `claude*` model ids, else 128000; an invalid value, or a window that leaves under 16000 characters of context after the reply reserve, exits `2`), `GatewayConfig::context_window_tokens`, `GatewayConfig::reply_reserve_tokens` and `LoopBudget::for_window` (#80).

### Changed

- `reqwest` gains the `gzip`, `brotli`, `zstd`, `deflate` features. The search legs (`web::search::ddg`/`startpage`) and the fetch static path (`web::fetch::fetcher`) both send an explicit `Accept-Encoding` matching the active browser profile (Chrome: `gzip, deflate, br, zstd`) (#57); the fetch static path draws its own profile per fetch rather than sharing one across a request chain (#74).
- `apply_browser_headers` is replaced by `apply_navigation_headers`, sending headers in Chrome's navigation order (`sec-ch-ua*`, `upgrade-insecure-requests`, `user-agent`, `accept`, `sec-fetch-*`, `accept-encoding`, `accept-language`, then `referer`/`origin`/`content-type` where Chrome sends them); `sec-fetch-site` is `none` only for a request with no initiating page in its chain (the Startpage homepage GET, when reached) and `same-origin` for every other request (every DDG request is a form POST from a page, so it is never `none`). `reqwest` still appends `Host` (and `Content-Length` on a body) after every header this crate sets; unavoidable without the #56 transport (#58).
- `is_startpage_challenge` also detects the Anubis proof-of-work challenge page (`id="anubis_challenge"`, `/.within.website/x/cmd/anubis/`) and a final URL under `/sp/cdn/error-pages/blocked`, alongside the existing `/sp/captcha`/`component---src-pages-captcha` markers; the bare word `anubis` in a result snippet never triggers it (#53).
- `SearchProviderError::AllFailed` carries `all_challenged: bool`; `research::agent_loop` maps an all-Challenge `search` failure to `ToolResult::SearchBlocked` instead of the generic `Failed` (#53). Extended (#62): a `Suspended` skip whose `reason` is `"challenge"` now counts toward `all_challenged` the same as a live `Challenge`, so an all-suspended fan-out also reaches the `SearchBlocked` exit-7 path; a `Suspended` skip from any other reason (5xx, transport, 429, 403, `Retry-After`), and the new `Throttled` skip, never do.
- `docs/harness.md`: exit code `7` documents the new `SearchBlocked` failure, extended for engine suspension (#62); a new "Engine governor" section documents the `SEARCH_*` suspension/pacing/allowlist env vars; a new "Browser profile bump" section documents the #59 table-refresh procedure.
- `web::fetch::fetcher::Fetcher` draws one Chrome browser profile per static fetch (`web::profile::pick_profile`) and sends it through `web::search::apply_navigation_headers` as a direct navigation (`ChainPosition::First`, no Referer): `sec-fetch-site: none`, the profile's coherent UA/`sec-ch-ua`/client-hint set, in Chrome's real navigation header order. Replaces the fixed `BROWSER_USER_AGENT` constant (Chrome 126), which is removed from `web::mod` — nothing else used it (#74).
- ADR-0008: transport stays `reqwest`; no own Chrome-impersonation build, no `wreq`, no `primp` for now, `primp` preferred over `wreq` if a swap is ever needed later, #60 stays open (#56).
- The Web Search agent's model-facing text is rewritten from a measured evaluation (#72, `docs/research/agent-prompt.md`):
  - System prompt (`research::prompt::build_system_prompt`, which now also takes the run's `LoopBudget`): a role line; Evidence rules (a search snippet is not Evidence, fetch before answering, cite only fetched pages, never guess a URL); a search, choose, fetch, check, answer workflow with a pages-per-size target (small 1-2, medium 2-4, large and deep 3-6); the turn budget and what to do when a fetch fails, search is blocked, or a page is cut; the answer shape `parse_answer` reads, with one worked example. It is built once per run and stays byte-identical across turns, and a section that directs a tool appears only when that tool is offered.
  - After every tool turn the newest tool result ends with the turns left, counting the final answer; on the last turn it tells the model to answer now. The empty-reply repair carries the same note, and on the last turn it asks only for the answer. The note rides in the transcript tail so the prompt-cache prefix holds, and it never enters recorded Evidence.
  - Observations: search Hits render as numbered candidates under "Candidates only, not Evidence" instead of `[title](url)` citation links; a fetch result starts with `Source: <final URL>`, so the Evidence cap no longer cuts it (it did in 17 of 18 measured fetches); failed-fetch, blocked-search and no-Hits results say what to do next.
  - Tool descriptions: `search` is engine-neutral (it named only DuckDuckGo and Startpage) and says only some engines apply `recency`; `fetch` says what it returns, that long pages are cut, and that `FAILED:` means fetch a different Hit.
  - The final answer goes inside an `<answer>` … `</answer>` block, each tag on a line of its own, and `parse_answer` parses only the last such block, so a progress remark around it ("I have enough information…") no longer becomes `synthesis.summary`. A tag counts only alone on its line, in any letter case, so a tag mentioned in a remark or shown in inline code stays text. A reply cut before its closing line parses from the opening line to the end; a reply with no opening line parses whole, as before.
- Fetched pages are no longer cut to 4,000 characters (#80). A long page is delivered in parts of up to `max_part_chars` (24,000 by default) and the result says `Part k of N`; the agent reads the rest with `fetch` and `part`. The registry keeps every fetched page whole for the run, so a continuation sends no HTTP request. `LoopBudget::max_evidence_chars` is now `max_part_chars`. The total context is sized from the model's window instead of a fixed 24,000 characters: `min((window - reply reserve) * 3, 400000)` characters, where the reserve is `GATEWAY_MAX_TOKENS` (else 8192) plus `GATEWAY_THINKING_BUDGET`. `--size` does not scale it. See `docs/harness.md` "Page parts and context budget".
  - The Session persists the whole fetched page once per URL, not the delivered part; `evidence_urls` lists each fetched URL once. `ResearchResponse` JSON is unchanged.
  - A repeat `fetch` of a known URL without `part` still returns `ALREADY FETCHED`, and now says how many parts the page has.
  - The `fetch` tool description and the system prompt no longer say long pages are cut; they explain `Part k of N` and `part`.

### Fixed

- The live Startpage Anubis proof-of-work challenge page used to parse as 0 rows instead of mapping to `Challenge` (429): none of the older markers (`/sp/captcha` redirect, `component---src-pages-captcha`) appear on that page (#53).
- A run whose search calls were all bot-wall-Challenged used to finalize `Ok` with an empty "I found nothing" Synthesis and exit `0`; it now surfaces `ResearchError::SearchBlocked` and exits `7` when the run has zero fetched Evidence, no `search` call ever returned a Hit, and at least one `search` call was fully Challenge-walled (#53).
- The agent could answer from search snippets without fetching a page, returning a Synthesis with `evidence_urls: []` whose links were dropped from `citations` (#72). On 7 fixed goals with recorded Hits (`docs/research/agent-prompt.md`), runs that fetched at least one page went from 6/7 to 7/7, runs that ran out of turns from 1 to 0, dropped citations from 3 of 13 to 0 of 14, and summaries opening with a progress remark or a heading from 6/6 to 0/7. The loop still does not enforce fetching: the prompt and observation changes met the bar on their own, so no guard was added.

## [0.2.0] - 2026-09-27

First working release: one CLI **Web Search** agent (`web-agent-research research "<goal>" --json`) that Harnesses call and that returns only the Synthesis. Verified live end to end (`docs/e2e-evidence.md`).

### Added

- `research` CLI subcommand and `run_research` library entry point (#15): `--size small|medium|large` (default `medium`), `--max-turns` (default `8`), `--session-id`, `--session-out`, `--json`. Exit codes: `2` not configured, `3` gateway exhausted, `4` tool failures, `5` turn budget, `6` Session I/O.
- LLM gateway client (#11): own `reqwest` client, no SDK, with retry/backoff, `Retry-After`, typed `GatewayError`, and native function calling. Config comes from `GATEWAY_*` env vars; the API key is never logged.
- Native Anthropic Messages API (#47), via `GATEWAY_API_FORMAT=anthropic`:
  - Sends `POST {base}/messages` with `x-api-key` and `anthropic-version: 2023-06-01`.
  - Uses a top-level `system`, one assistant `tool_use` message, then one user message of `tool_result` blocks.
  - `thinking`/`redacted_thinking` blocks are sent back verbatim on the next turn.
  - Prompt caching through up to three `cache_control` breakpoints: last tool, last system block, newest message block.
  - Maps `thinking.budget_tokens` (minimum 1024) and `output_config.effort`; `max_tokens` defaults to 8192 plus the thinking budget.
  - Maps `stop_reason` to finish reasons; a `refusal` fails the turn.
  - Drops whitespace-only text blocks. ADR-0007 records the design.
- Standard request params (#41):
  - `GATEWAY_REASONING_EFFORT`, validated against `none|minimal|low|medium|high|xhigh|max`.
  - `GATEWAY_THINKING_BUDGET` and `GATEWAY_MAX_TOKENS`.
  - `prompt_cache_key` set to the Session id on every turn; `GATEWAY_PROMPT_CACHE_KEY=off` disables it.
  - `GATEWAY_EXTRA_BODY` passthrough; typed fields win on key collisions.
- Token usage in `--json` and Session rows: `cached_prompt_tokens` (cache reads, from the OpenAI and Anthropic shapes) and `cache_creation_prompt_tokens` (cache writes), summed across turns. Both are `#[serde(default)]`, so older Session rows still parse.
- Multi-turn agent loop (#12):
  - Offers tools as native `tool_calls` and gates them against `allowed_tools`.
  - Caps tool calls per turn and applies a per-tool timeout.
  - Runs a repair budget for dispatch and execution failures.
  - Truncates history by whole turns and parses the answer into a typed Synthesis.
- Native tool-role transcript (#42): one assistant `tool_calls` message plus one `tool` message per call, paired by id and append-only, so provider prompt caches can reuse the prefix.
- Search (#13): DuckDuckGo HTML and Startpage legs, a parallel multi-query fan-out with `dedup_key` + consensus merge, and typed `SearchProviderError`s.
- Fetch (#31):
  - `reqwest` + `htmd` static fetch first.
  - Falls back to the Obscura browser only for challenge-marked 403/429/503, 200 challenge pages, JS shells, or non-text content.
  - Enforces a streamed 5 MiB cap.
  - Records `Evidence.fetch_path` (`static`/`browser`) in Session rows.
- Per-run fetch dedup (#43): a repeat `fetch` of a URL already read returns `AlreadyFetched` with no I/O, and search Hits already fetched are marked.
- `docs/harness.md` (#33): the Harness contract (command, env table, `--json` shape, exit codes, measured latency, instruction snippet).
- `docs/e2e-evidence.md` (#33): a live real-tools run on `claude-haiku-4.5` via kiro-backend, plus a fresh-Harness acceptance check.
- ADR-0006 (one product, `web/`/`llm/`/`research/` layout, `research` depends on the other two and never the reverse) and ADR-0007 (wire format as an enum branch). The glossary adds Web Search, Harness, Hit.

### Changed

- Module layout moved from `src/shared/` + `src/slices/` to `src/web/`, `src/llm/`, `src/research/` (#30). ADR-0006 supersedes ADR-0004.
- `synthesis.citations` and theme citations keep only pages fetched as Evidence in the run, so every citation a Harness sees was actually read.
- Only fetched pages count as Evidence and appear in `evidence_urls`; search Hits never do (#32).
- An HTTP 403 from the gateway is `GatewayError::Forbidden` with the upstream message, instead of "bad API key"; 401 stays `Auth`.
- The default `GATEWAY_MODEL` is `muse-spark-1.3`. Live tests (`GATEWAY_LIVE=1`) use one model, with no fallback chain.
- `AGENTS.md`, README, and the PR template are in English.

### Removed

- The `ToolRegistry::live()` canned stubs and the duplicate registry schemas/types (`SearchHit`, `FetchedPage`) (#25, #26).
- The `TOOL RESULTS:`/`TOOL ERROR:` text protocol in the transcript.
- The root `PLAN.md` and the empty `classify`/`research` placeholders.

### Fixed

- `Evidence.fetch_path` was always `null` in Session rows.
- Agent loop reviewer fixes (#28): allowed-tools gate, per-call atomicity, failure-counter reset, and `AnswerError` typing.

### Known issues

- DuckDuckGo (anomaly page) and Startpage (proof-of-work challenge) can wall search from some IPs; the Startpage page is not yet detected as a challenge (#53).

## [0.1.0] - 2026-09-08

First versioned release: CLI prototype scaffold with the domain core in place.

### Added

- CLI binary (`clap` + `tokio`) accepting a research goal argument; `--version` reads from `Cargo.toml`.
- Library core (`src/lib.rs`) holding all domain logic; `SESSION_FORMAT_VERSION = 1` for the Session JSONL format.
- `shared/` foundation: `agent_loop`, `obscura`, `tools`, `prompts`, `types`, `session` (Session in `sessions/<id>.jsonl` + on-disk HTTP cache, no DB).
- `slices/search/` scaffold (handler stub, wired); `deep` Mode reserved as a sibling folder.
- Domain docs: `CONTEXT.md` glossary (Research, Session, Turn, Evidence, Mode, Synthesis) in English; ADRs 0001–0005.
- CI (`ci.yml`): `lint` (fmt + clippy) and `test` (test + build `--locked`) on push/PR to `main`.
- Main PR-only enforcement (`protect-main.yml` post-push detection + `.githooks/pre-push` local block) — see ADR-0005.

### Changed

- Session persistence collapsed to a single `session.rs` file; `schemas/` removed.
- `CONTEXT.md` glossary and README translated to English.
- CI hardened: root-commit bypass closed, `force-with-lease` removed, `test --all-targets`.

### Fixed

- Wired `mod.rs` (`pub mod`) so the scaffold actually compiles.

[Unreleased]: https://github.com/levygit837-cyber/web-agent-research/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/levygit837-cyber/web-agent-research/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/levygit837-cyber/web-agent-research/releases/tag/v0.1.0
