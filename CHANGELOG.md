# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `ToolRegistry` per-run fetch dedup (#43): the live registry remembers every URL successfully fetched this Research, keyed by `dedup_key` (both the requested URL and the final post-redirect `Evidence.source_url`). A repeat `fetch` of a known key does no network I/O and returns `ToolResult::AlreadyFetched` (success, no Evidence, `url()` is `None`) pointing the model at the earlier fetch; a failed fetch is never recorded, so a retry still reaches the server. Search Hits already fetched this run render with an `(already fetched)` marker so the model picks new ones.
- `docs/harness.md` (#33): CLI contract for Harnesses (command, full `GATEWAY_*` env table with defaults and invalid-value exit codes, `--json` shape, exit codes, a measured Latency section for search/fetch/browser-fallback tool calls, instruction snippet); linked from README.
- Standard reasoning, thinking, and prompt-cache request params (#41): `ReasoningEffort` enum (`none|minimal|low|medium|high|xhigh|max`) replaces the unvalidated `reasoning_effort` passthrough string, invalid `GATEWAY_REASONING_EFFORT` exits `2`; `GATEWAY_THINKING_BUDGET` sends Anthropic/DeepSeek-shape `thinking: {type: "enabled", budget_tokens}` or `{type: "disabled"}` for `0`, rejected when `>= GATEWAY_MAX_TOKENS`; `prompt_cache_key` is the run's Session id (generated before the loop, not after) on every turn, `GATEWAY_PROMPT_CACHE_KEY=off` disables it; `GATEWAY_EXTRA_BODY` merges a JSON object into the wire body (OpenAI SDK `extra_body` semantics, typed fields win on collision), invalid JSON or a non-object exits `2`. `TokenUsage`/`UsageDTO`/`SessionUsage` add `cached_prompt_tokens` (max of OpenAI's `usage.prompt_tokens_details.cached_tokens` and the Anthropic/LiteLLM `usage.cache_read_input_tokens`), `#[serde(default)]` so pre-#41 Session JSONL still parses; new `TokenUsage::add` gives the agent loop one per-field saturating sum instead of four manual `saturating_add` call sites.
- Research agent calls the real web tools (#32): `ToolRegistry::new(Searcher, Fetcher)` dispatches `search` to the DDG + Startpage fan-out and `fetch` to `Fetcher`; schemas come from `web/`. Only fetched pages become Evidence; search Hits no longer count as evidence URLs.
- `web::fetch::Fetcher` (#31): `reqwest` + `htmd` static fetch first; Obscura fallback only on a 403/429/503 whose body or `cf-mitigated: challenge` header marks it a challenge/anti-bot page (a plain 403/429/503, e.g. a real outage or paywall, returns `FetchError::Http` instead), challenge pages on 200, JS shells (<200 chars of markdown) or non-text content. Response bodies are read through a capped streaming reader (5 MiB), so an unbounded chunked response is rejected mid-stream instead of after a full buffer. Missing `obscura` binary → `FetchError::FallbackUnavailable`. New `FetchError::Http` for 404/5xx/transport/challenge-free 403-429-503/oversize. `Evidence.fetch_path` (`static`/`browser`, omitted when absent) records which engine produced the markdown. Live: docs.rs 0.25 s, wikipedia 0.63 s, react.dev 0.41 s, all static (was 1.5–8.2 s via Obscura).
- Fetch-only search tools (`web/search/tool.rs` + `web/search/types.rs`): DuckDuckGo HTML POST (form `q`/`kl`/`df`/`b`, `s`+`vqd` continuation, regex parse) and Startpage (`sc`-token flow with direct-GET fallback, DOM parse), parallel multi-query fan-out with `dedup_key` + consensus `merge_sources`, `SearchProviderError` mapping (429 challenge / 504 timeout / 503 upstream + `AllFailed`), and a validated agent tool schema.

### Changed

- Default `GATEWAY_MODEL` is now `muse-spark-1.3` (was `glm-5p2`). Live tests (`GATEWAY_LIVE=1`) use one model — `GATEWAY_LIVE_MODEL`, else `GATEWAY_MODEL`, else the default — with no `glm-5p2`/`mimo-v2.5-free` fallback chain.
- Removed `ToolRegistry::live()` canned stubs, registry-local `query`/`max_chars` schemas, `SearchHit`, `FetchedPage` (closes #25, #26).
- Agent loop transcript moved to the native OpenAI tool-role shape (#42): `llm::ChatMessage` gains `tool_calls`/`tool_call_id` fields plus ergonomic constructors (`system`/`user`/`assistant`/`assistant_tool_calls`/`tool`); each tool turn now appends one assistant message carrying the model's own `tool_calls` (ids preserved, synthesized as `call_<turn>_<index>` when the model omits one) followed by one `role: "tool"` message per call, in wire order — including failed, timed-out, disallowed, and per-turn-cap-dropped calls, so every id always pairs. History truncation now drops whole turns oldest-first, never an orphan `tool` message or unpaired `tool_calls`. Removed the `TOOL RESULTS:`/`TOOL ERROR:` text protocol; `ToolResult::Failed` carries a typed `kind: FailureKind` (`Dispatch`/`Execution`) instead of string-matching the rendered reason. System prompt and tool defs are now byte-identical turn to turn and history is strictly append-only (aside from truncation), so provider prompt caching can reuse the prefix.
- `AGENTS.md`, README intro, and the PR template rewritten in English; repo content stays English, chat replies follow the session language.
- ADR-0006 supersedes ADR-0004: one product (the Web Search agent via CLI), modules `web/`, `llm/`, `research/` where `research` depends on the other two and never the reverse, reqwest-first fetch with Obscura fallback. Glossary adds Web Search, Harness, Hit.
- `src/shared/` + `src/slices/` replaced by `src/web/`, `src/llm/`, `src/research/` (#30); `Evidence` moves to `web::fetch`; empty `classify`/`research` placeholders removed. No behavior change. Removed root `PLAN.md` (closed #11 plan for the pre-ADR-0006 `shared/gateway` layout); per-issue plans live in git-ignored `.plans/`.

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

[Unreleased]: https://github.com/levygit837-cyber/web-agent-research/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/levygit837-cyber/web-agent-research/releases/tag/v0.1.0
