# web-agent-research

CLI prototype of a multi-turn web research agent that extracts and synthesizes multiple pages on the [Obscura](https://github.com/h4ckf0r0day/obscura) engine, evolving into an API without rewrite.

Private repo: `https://github.com/levygit837-cyber/web-agent-research`

## Stack (recorded decisions)

- **Rust + tokio** as anchor language — [ADR-0001](docs/adr/0001-rust-linguagem-ancora.md)
- **No DB in the prototype**: Session in `sessions/<id>.jsonl` + on-disk HTTP cache — [ADR-0002](docs/adr/0002-sem-db-so-arquivos.md)
- **Own OpenAI-compatible HTTP LLM** (`reqwest` + `serde`, no SDK) — [ADR-0003](docs/adr/0003-http-openai-compatible-proprio.md)
- **Obscura first via subprocess** (`obscura fetch`) / CDP; native client out of scope

## Quickstart

```bash
cargo run -- "what is the Obscura headless browser?"
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI on push/PR to `main`: `ci.yml` (`lint`: fmt + clippy; `test`: test + build) and `protect-main.yml` (fails if any commit landed without a PR). Every change goes through a PR — see [ADR-0005](docs/adr/0005-protecao-main-sem-plano-pago.md). After cloning, enable the local direct-push block: `git config core.hooksPath .githooks`.

## Structure

- `src/lib.rs` — agent core (all domain logic lives here)
- `src/main.rs` — thin CLI shell over the lib
- `src/shared/` — agent_loop, obscura, tools, prompts, types, session
- `src/slices/` — one folder per Mode (`search` now, `deep` later)
- `sessions/`, `cache/` — created at runtime, out of git
- `CONTEXT.md` — domain glossary (Research, Session, Turn, Evidence)
- `docs/adr/` — architecture decisions
- `docs/agents/` — engineering skills config (issue tracker, domain docs)
- `docs/research/` — supporting research
- `CHANGELOG.md` — version history (Keep a Changelog + SemVer)

## Issues

GitHub Issues via `gh`. See `docs/agents/issue-tracker.md`.
