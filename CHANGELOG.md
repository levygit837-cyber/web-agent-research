# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `docs/harness.md` (#33): CLI contract for Harnesses (command, env, `--json` shape, exit codes, instruction snippet); linked from README.
- Research agent calls the real web tools (#32): `ToolRegistry::new(Searcher, Fetcher)` dispatches `search` to the DDG + Startpage fan-out and `fetch` to `Fetcher`; schemas come from `web/`. Only fetched pages become Evidence; search Hits no longer count as evidence URLs.
- `web::fetch::Fetcher` (#31): `reqwest` + `htmd` static fetch first; Obscura fallback only on HTTP 403/429/503, challenge pages, JS shells (<200 chars of markdown) or non-text content. Missing `obscura` binary → `FetchError::FallbackUnavailable`. New `FetchError::Http` for 404/5xx/transport/oversize (5 MiB cap). Live: docs.rs 0.25 s, wikipedia 0.63 s, react.dev 0.41 s, all static (was 1.5–8.2 s via Obscura).
- Fetch-only search tools (`shared/tools/search.rs` + `shared/types/search.rs`): DuckDuckGo HTML POST (form `q`/`kl`/`df`/`b`, `s`+`vqd` continuation, regex parse) and Startpage (`sc`-token flow with direct-GET fallback, DOM parse), parallel multi-query fan-out with `dedup_key` + consensus `merge_sources`, `SearchProviderError` mapping (429 challenge / 504 timeout / 503 upstream + `AllFailed`), and a validated agent tool schema.

### Changed

- Removed `ToolRegistry::live()` canned stubs, registry-local `query`/`max_chars` schemas, `SearchHit`, `FetchedPage` (closes #25, #26).
- `AGENTS.md`, README intro, and the PR template rewritten in English; repo content stays English, chat replies follow the session language.
- ADR-0006 supersedes ADR-0004: one product (the Web Search agent via CLI), modules `web/`, `llm/`, `research/` where `research` depends on the other two and never the reverse, reqwest-first fetch with Obscura fallback. Glossary adds Web Search, Harness, Hit.
- `src/shared/` + `src/slices/` replaced by `src/web/`, `src/llm/`, `src/research/` (#30); `Evidence` moves to `web::fetch`; empty `classify`/`research` placeholders removed. No behavior change.

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
