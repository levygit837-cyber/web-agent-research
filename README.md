# web-agent-research

CLI **Web Search** tool for coding-agent Harnesses: one call runs a multi-turn research agent (search → fetch → synthesize) and returns only the Synthesis, so the calling model never spends context on raw pages. Architecture: [ADR-0006](docs/adr/0006-web-search-tool-architecture.md).

Harness integration (command, JSON shape, exit codes): [docs/harness.md](docs/harness.md).

Private repo: `https://github.com/levygit837-cyber/web-agent-research`

## Stack (recorded decisions)

- **Rust + tokio** as anchor language — [ADR-0001](docs/adr/0001-rust-linguagem-ancora.md)
- **No DB in the prototype**: Session in `sessions/<id>.jsonl` + on-disk HTTP cache — [ADR-0002](docs/adr/0002-sem-db-so-arquivos.md)
- **Own OpenAI-compatible HTTP LLM** (`reqwest` + `serde`, no SDK) — [ADR-0003](docs/adr/0003-http-openai-compatible-proprio.md)
- **reqwest-first fetch, Obscura fallback** for JS/blocked pages — [ADR-0006](docs/adr/0006-web-search-tool-architecture.md)

## Quickstart

```bash
cargo run -- research "what is the Obscura headless browser?" --json
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI on push/PR to `main`: `ci.yml` (`lint`: fmt + clippy; `test`: test + build) and `protect-main.yml` (fails if any commit landed without a PR). Every change goes through a PR — see [ADR-0005](docs/adr/0005-protecao-main-sem-plano-pago.md). After cloning, enable the local direct-push block: `git config core.hooksPath .githooks`.

## Structure

- `src/main.rs` — thin CLI shell over the lib
- `src/lib.rs` — public interface: `run_research`
- `src/web/` — search (Hits) + fetch (Evidence); `src/llm/` — gateway; `src/research/` — agent loop, prompt, Synthesis, Session. Dependency rule: [ADR-0006](docs/adr/0006-web-search-tool-architecture.md)
- `sessions/`, `cache/` — created at runtime, out of git
- `CONTEXT.md` — domain glossary (Research, Session, Turn, Evidence)
- `docs/adr/` — architecture decisions
- `docs/agents/` — engineering skills config (issue tracker, domain docs)
- `docs/research/` — supporting research
- `CHANGELOG.md` — version history (Keep a Changelog + SemVer)

## Issues

GitHub Issues via `gh`. See `docs/agents/issue-tracker.md`.
