# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `web::search::cache`: on-disk cache for search legs, keyed by normalized query (trim, collapse whitespace, lowercase) + recency + engine + page (#67). Entries live under `<cache_root>/search/`, one file per leg, atomic writes via `cache_dir::write_atomic`. Cached per engine leg, not per merged fan-out output, so a cached leg for one engine and a live leg for another still merge normally. TTL defaults to 24h, 1h when the Query's `recency` is `day`; `SEARCH_CACHE_TTL_SECS` overrides both uniformly; `SEARCH_CACHE=off` disables lookup and store outright. Only a settled `Ok` leg is ever stored: a `Challenge`/`Timeout`/`Upstream` leg error has no code path into the cache. A zero-row result is cached like any other successful leg. Size-capped (`SEARCH_CACHE_MAX_BYTES`, default 20 MiB) with oldest-first eviction by `fetched_at`, run automatically after every `store`. A corrupt entry is treated as already expired (ignored on lookup, evicted first, overwritten on the next `store` at the same key) rather than failing the leg.
- `web::cache_dir`: shared cache-root resolution (`SEARCH_CACHE_DIR`, else `XDG_CACHE_HOME/web-agent-research`, else `HOME/.cache/web-agent-research`, else disabled) and the atomic-write helper, shared by the search cache and engine-governor state (#62, #67).

### Changed

- `is_startpage_challenge` also detects the Anubis proof-of-work challenge page (`id="anubis_challenge"`, `/.within.website/x/cmd/anubis/`) and a final URL under `/sp/cdn/error-pages/blocked`, alongside the existing `/sp/captcha`/`component---src-pages-captcha` markers; the bare word `anubis` in a result snippet never triggers it (#53).
- `SearchProviderError::AllFailed` carries `all_challenged: bool`; `research::agent_loop` maps an all-Challenge `search` failure to `ToolResult::SearchBlocked` instead of the generic `Failed` (#53).
- `docs/harness.md`: exit code `7` documents the new `SearchBlocked` failure.

### Fixed

- The live Startpage Anubis proof-of-work challenge page used to parse as 0 rows instead of mapping to `Challenge` (429): none of the older markers (`/sp/captcha` redirect, `component---src-pages-captcha`) appear on that page (#53).
- A run whose search calls were all bot-wall-Challenged used to finalize `Ok` with an empty "I found nothing" Synthesis and exit `0`; it now surfaces `ResearchError::SearchBlocked` and exits `7` when the run has zero fetched Evidence, no `search` call ever returned a Hit, and at least one `search` call was fully Challenge-walled (#53).

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
