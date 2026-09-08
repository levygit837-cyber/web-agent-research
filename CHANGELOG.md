# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
