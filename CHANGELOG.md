# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking

- Sessions default to `<data root>/sessions/<id>.jsonl` (`$WEB_AGENT_RESEARCH_HOME`, else `$XDG_DATA_HOME/web-agent-research`, else `~/.local/share/web-agent-research`), not `./sessions/`; a failed defaulted write warns and exits `0`, exit `6` is only for an explicit `--session-out` (#102).
- `--session-id` must match `[A-Za-z0-9._-]{1,64}` with no leading dot; anything else exits `2` (#102).
- `--json` adds `content_trust: "untrusted-web"`, `turn_budget`, citation `support` (`exact`/`partial`/`none`/`unchecked`) and `quote`, Synthesis `verification` counts, and Evidence `fetch_path: "api"`; the Harness snippet treats the Synthesis as untrusted web content (#98, #101, #106, #109).
- `fetch` refuses loopback, private, link-local and other reserved addresses, incl. via DNS and redirects (#97).
- Exit `8` for requests the gateway rejects (401, 403, other 4xx, error-in-200, empty choices, refusal), `3` is retryable exhaustion only, `9` is a deadline with no answer, a malformed `GATEWAY_BASE_URL` exits `2`, and exit `7` names each engine once (#99, #100, #104).
- `--deadline-secs` (default 300) bounds the run; `--max-turns` defaults to 10, and each successful search after the first adds 5 turns up to `--max-turns-cap` (default 25) (#100, #101).
- The final turn sends `tool_choice: none`; logs go to stderr, so stdout is only the response (#99, #101).
- `SEARCH_ENGINES` defaults to `duckduckgo,brave,yahoo,bing,cratesio`: the new crates.io vertical is on by default and joins only crate-shaped Queries (#109).
- Rust API: `FetchError::Egress`; `Fetcher::with_policy` and `Obscura::with_egress` take an `EgressPolicy`; `GatewayError::{Client, Server}` carry `detail`; `LoopBudget` adds `max_turns_cap`, `deadline`, `final_reserve`; `SearchOutput` and `ToolResult::Search` add `engine_status`; `SearchProvider::CratesIo`; `Searcher::with_bases` and `search_multi_with_bases` take a crates.io base (#97, #99, #100, #101, #104, #109).

### Added

- Session files are created with mode `0600`; `SESSION_MAX_BYTES` (default 256 MiB) evicts the oldest Sessions (#102).
- Gateway retries honor `Retry-After` (seconds or HTTP-date) on 429/503/529 with full-jitter backoff; error messages carry the upstream detail (#99).
- Model-free grounding check: each cited bullet's verbatim quote and its numbers, versions, error codes and identifiers are checked against the cited page (#106).
- Human output marks sources whose page does not support the cited bullets as `(unsupported: …)` or `(partial: …)` (#106).
- `FETCH_ALLOW_PRIVATE=1` lets `fetch` reach private addresses, for local testing (#97).
- Offline eval: golden set, eval-only `WAR_EVAL_RECORD`/`WAR_EVAL_REPLAY` tool tape, judge, paired compare (#107).
- crates.io search leg: crate-shaped Queries return a docs.rs Hit (`<name> <version>`, description) from the keyless API, 1 request/s, with an identifying User-Agent; `WEB_AGENT_RESEARCH_CONTACT` sets its contact (#109).
- `fetch` serves `crates.io/crates/<name>` from the sparse index and API as a markdown block (versions, yanked, `rust_version`, links), without Obscura (#109).
- eval: `WAR_EVAL_REPLAY_THROUGH=<dir>` and `eval run --mode replay-through` replay `search`, fetch unrecorded pages live and append them to `pages.jsonl` (#129).
- eval: `eval judge` adds a claim pass that checks each bullet against its cited pages; `claim_precision`, `claim_partly`, `claim_unsupported`, `claim_contradicted`, `uncited_share` in `summary` and `compare` (#123).

### Changed

- Page parts and Hits reach the agent inside per-run nonce containers it cannot close, and the prompt says their text is third-party data, never instructions (#98).
- The agent ends each cited bullet with `(quote: "…")`, copied verbatim from the cited page (#106).
- A crates.io Hit weighs 2 in the rank fusion, so its rank 1 is not buried behind a URL two engines return at rank 5 (#109).
- Static fetches keep only the page's `main`/`article` content when no code block, table row or answer is lost (#108).
- `search` Hits rank by Reciprocal Rank Fusion over engine x query legs, then query coverage, not engine count first (#110).
- A `search` call with no Hits tells the agent each engine's status: rows, no results, suspended, timed out or unrecognized markup (#104).
- After the 5 s soft deadline, legs already sending wait up to the 30 s hard deadline; only legs still queued are cancelled (#104).
- DuckDuckGo continuation pages are paced and counted against the engine's request budget (#104).

### Fixed

- `fetch` recognizes the Cloudflare "Performing security verification" interstitial on both paths: static hands over to Obscura, a still-interstitial rendered page fails with `Blocked`; long articles quoting the phrase are unaffected (#126).
- `search` accepts `queries` sent as a JSON-encoded array string, validated like an array (#128).
- `rust-version` said `1.75`, but the locked tree needs Rust 1.88; it now says `1.88` and CI checks it (#105).
- Obscura output is capped at 5 MiB per stream and the browser is killed on overflow (#97).
- Read timeouts on a 2xx body and error objects in 2xx bodies are classified correctly; `stream` is always `false`; URLs stay out of errors (#99).
- A hung gateway or tool no longer stretches a run: `connect_timeout` 10 s, one timeout retry, static fetch 10 s, Obscura 30 s (#100).
- Parallel runs on one `SEARCH_CACHE_DIR` no longer overwrite each other's `engines.json`: it is merged under a file lock (#103).
- `Retry-After` and stored suspensions are capped at the largest `SEARCH_SUSPEND_*_SECS` value (default 1 h) (#103).
- Transport errors without an HTTP status no longer suspend an engine; they are retried once (#103).
- Changed engine markup is a typed `unrecognized markup` error, not an empty answer, and zero-row legs are never cached (#104).
- Failed cache writes remove their temp file, and stale temp files are evicted (#103).

## [0.3.0] - 2026-10-04

Search runs over four free engines by default, long pages arrive whole in parts, and `recency` is honored per engine.

### Breaking

- Startpage is off by default; opt in with the new `SEARCH_ENGINES` allowlist (#64, #66).
- `web::search::search_multi` is removed; use `web::search::tool::Searcher::search` (#71).
- `LoopBudget::max_evidence_chars` is now `max_part_chars`; `ToolRegistry::execute` takes `part_chars`; `build_system_prompt` takes a `&LoopBudget` (#72, #80).
- Moved: `LoopInput`, `LoopBudget`, `ToolEvidence`, `RunReport`, `LoopError` from `agent_loop::runner` to `agent_loop::types` (#83).
- Moved: `ToolResult`, `FailureKind` from `agent_loop::registry` to `agent_loop::result` (#83).
- Moved: `ResearchRequest`, `ResearchResponse`, the `*DTO` types and `default_max_turns` from `research::run` to `research::dto` (#83).
- New variants: `ResearchError::SearchBlocked`, `SearchProviderError::{Suspended, Throttled}`, `SearchProvider::{Brave, Yahoo, Bing}` (#53, #62, #66).
- New variants: `ToolResult::SearchNote`, `FetchError::InvalidPart` (#80, #81).
- New fields: `GatewayConfig::context_window_tokens`, `FetchInput::part`, `SearchOutput::note` (#80, #81).
- New fields: `SearchProviderError::Upstream::{status, retry_after_secs}`, `AllFailed::all_challenged` (#53, #62).

### Added

- Brave, Yahoo and Bing search engines (#66).
- `SEARCH_ENGINES` picks the engines to query; default `duckduckgo,brave,yahoo,bing` (#64, #66).
- Engine governor: suspends walled engines, paces requests, caps them per run (#62, #63, #73).
- On-disk search cache with a 24 h TTL; `SEARCH_CACHE=off` disables it (#67).
- Chrome browser profiles with real header order and compression, for search and fetch (#57, #58, #59, #74).
- `fetch` with `part=k` reads long pages in parts, with no new HTTP request (#80).
- `GATEWAY_CONTEXT_WINDOW` sizes the context budget; a window too small exits `2` (#80).
- Pagination depth per engine: `SEARCH_MAX_PAGES[_<ENGINE>]` (#73).
- Exit code `7` when every engine is walled (#53).
- ADR-0008 (stay on `reqwest`) and ADR-0009 (file size and test layout) (#56, #82).

### Changed

- Agent prompt rewritten: fetch before answering, turns-left note, `<answer>` block (#72).
- Fetched markdown drops data URIs, image noise and tracking params (#79).
- Fetched pages are no longer cut at 4,000 characters (#80).
- Engine evaluation and per-engine limits recorded in `docs/research/search-engines.md` (#65, #73).

### Fixed

- Startpage's Anubis page is detected as a wall instead of 0 results (#53).
- An all-walled run exits `7` instead of returning an empty Synthesis (#53).
- The agent answered from snippets without fetching a page (#72).
- Brave, Yahoo and Bing ignored `recency`; Yahoo now applies it, the others are skipped (#81).

### Known issues

- Brave and DuckDuckGo wall this IP at times; the governor skips them.
- Brave's `recency` param is unverified, so Brave is skipped when `recency` is set.
- No Chrome TLS or HTTP/2 fingerprint yet (#60).

## [0.2.0] - 2026-09-27

First working release: one CLI Web Search agent that returns only the Synthesis.

### Added

- `research` subcommand and `run_research` entry point, with exit codes `2` to `6` (#15).
- LLM gateway client: OpenAI-compatible or native Anthropic Messages (#11, #47).
- Gateway params: reasoning effort, thinking budget, max tokens, prompt cache key, extra body (#41).
- Token usage, including cache reads and writes, in `--json` and Session rows.
- Multi-turn agent loop with native tool calls and a repair budget (#12, #42).
- DuckDuckGo and Startpage search with merged, deduped Hits (#13).
- Static fetch with an Obscura fallback for JS and walled pages (#31).
- A repeat `fetch` of a page already read sends no request (#43).
- Harness contract and live E2E evidence: `docs/harness.md`, `docs/e2e-evidence.md` (#33).
- ADR-0006 (module layout) and ADR-0007 (wire format enum).

### Changed

- Modules moved to `src/web/`, `src/llm/` and `src/research/` (#30).
- Only fetched pages count as Evidence or citations (#32).
- Gateway HTTP 403 is `GatewayError::Forbidden`, not "bad API key".
- Default `GATEWAY_MODEL` is `muse-spark-1.3`.

### Removed

- Canned `ToolRegistry::live()` stubs and duplicate schemas (#25, #26).
- The `TOOL RESULTS:` text protocol in the transcript.

### Fixed

- `Evidence.fetch_path` was always `null` in Session rows.
- Agent loop: allowed-tools gate, per-call atomicity, failure-counter reset (#28).

### Known issues

- DuckDuckGo and Startpage can wall search from some IPs (#53).

## [0.1.0] - 2026-09-08

First versioned release: CLI prototype scaffold with the domain core in place.

### Added

- CLI binary that takes a research goal; `--version` reads `Cargo.toml`.
- Library core in `src/lib.rs`; Session JSONL format version 1.
- `shared/` foundation and `slices/search/` scaffold.
- `CONTEXT.md` glossary and ADRs 0001 to 0005.
- CI: `lint` and `test` on push and PR to `main`.
- PR-only `main`: `protect-main.yml` and `.githooks/pre-push` (ADR-0005).

### Changed

- Session persistence collapsed into `session.rs`.
- CI hardened: root-commit bypass closed, `test --all-targets`.

### Fixed

- `mod.rs` wiring, so the scaffold compiles.

[Unreleased]: https://github.com/levygit837-cyber/web-agent-research/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/levygit837-cyber/web-agent-research/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/levygit837-cyber/web-agent-research/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/levygit837-cyber/web-agent-research/releases/tag/v0.1.0
