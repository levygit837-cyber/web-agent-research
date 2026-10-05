# web-agent-research

CLI **Web Search** tool for coding-agent Harnesses: one call runs a multi-turn research agent (search → fetch → synthesize) and returns only the Synthesis, so the calling model never spends context on raw pages. Architecture: [ADR-0006](docs/adr/0006-web-search-tool-architecture.md).

Harness integration (command, JSON shape, exit codes): [docs/harness.md](docs/harness.md).

Private repo: `https://github.com/levygit837-cyber/web-agent-research`

## Stack (recorded decisions)

- **Rust + tokio** as anchor language — [ADR-0001](docs/adr/0001-rust-linguagem-ancora.md)
- **No DB in the prototype**: Session in `<data root>/sessions/<id>.jsonl`; search-leg cache and engine state on disk under the cache root — [ADR-0002](docs/adr/0002-sem-db-so-arquivos.md)
- **Own HTTP LLM client** (`reqwest` + `serde`, no SDK): OpenAI-compatible — [ADR-0003](docs/adr/0003-http-openai-compatible-proprio.md) — or native Anthropic Messages via `GATEWAY_API_FORMAT` — [ADR-0007](docs/adr/0007-llm-wire-format-enum.md)
- **reqwest-first fetch, Obscura fallback** for JS/blocked pages — [ADR-0006](docs/adr/0006-web-search-tool-architecture.md)
- **Transport stays `reqwest`**: no own Chrome-impersonation build, no `wreq`, no `primp` for now — [ADR-0008](docs/adr/0008-search-transport-reqwest.md)
- **Files split by responsibility**: production code ~500 lines per file, oversized test modules in `<stem>/tests/<theme>.rs` — [ADR-0009](docs/adr/0009-file-size-test-layout.md)

## Quickstart

```bash
set -a; . ./.env; set +a   # GATEWAY_* from the git-ignored .env (see docs/harness.md)
cargo run -- research "what is the Obscura headless browser?" --json
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI on push/PR to `main`: `ci.yml` (`lint`: fmt + clippy; `test`: test + build) and `protect-main.yml` (fails if any commit landed without a PR). Every change goes through a PR — see [ADR-0005](docs/adr/0005-protecao-main-sem-plano-pago.md). After cloning, enable the local direct-push block: `git config core.hooksPath .githooks`.

## Structure

- `src/main.rs` — thin CLI shell over the lib
- `src/lib.rs` — public interface: `run_research`
- `src/web/` — search (Hits from DuckDuckGo, Brave, Yahoo, Bing, opt-in Startpage) + fetch (Evidence, delivered in parts); `src/llm/` — gateway; `src/research/` — agent loop, prompt, Synthesis, Session. Dependency rule: [ADR-0006](docs/adr/0006-web-search-tool-architecture.md)
- Two per-user roots, created at runtime, never the current directory (see `docs/harness.md`, "Data root and Sessions"): Sessions live under the data root, `$WEB_AGENT_RESEARCH_HOME`, else `$XDG_DATA_HOME/web-agent-research`, else `~/.local/share/web-agent-research`, in `sessions/` (mode `0600`, capped by `SESSION_MAX_BYTES`, oldest evicted); the search cache and `engines.json` live under the cache root, `$SEARCH_CACHE_DIR`, else `$XDG_CACHE_HOME/web-agent-research`, else `~/.cache/web-agent-research`
- `CONTEXT.md` — domain glossary (Research, Session, Turn, Evidence)
- `docs/adr/` — architecture decisions
- `docs/agents/` — engineering skills config (issue tracker, domain docs)
- `docs/research/` — supporting research
- `CHANGELOG.md` — version history (Keep a Changelog + SemVer)
- `THIRD-PARTY-NOTICES.md` — attribution and license text for ported code (oh-my-pi, Obscura)

## Issues

GitHub Issues via `gh`. See `docs/agents/issue-tracker.md`.
