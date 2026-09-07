# web-agent-research

Protótipo CLI de agente multi-turno para pesquisa na web sobre a engine [Obscura](https://github.com/h4ckf0r0day/obscura), evoluindo para API sem rewrite.

Repo privado: `https://github.com/levygit837-cyber/web-agent-research`

## Stack (decisões registradas)

- **Rust + tokio** como linguagem âncora — [ADR-0001](docs/adr/0001-rust-linguagem-ancora.md)
- **Sem DB no protótipo**: Sessão em `sessions/<id>.jsonl` + cache HTTP em disco — [ADR-0002](docs/adr/0002-sem-db-so-arquivos.md)
- **LLM via HTTP OpenAI-compatible próprio** (`reqwest` + `serde`, sem SDK) — [ADR-0003](docs/adr/0003-http-openai-compatible-proprio.md)
- **Obscura primeiro via subprocesso** (`obscura fetch`) / CDP; cliente nativo fora do escopo

## Quickstart

```bash
cargo run -- "o que é Obscura headless browser?"
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI (`.github/workflows/ci.yml`) roda fmt + clippy + test + build em push/PR na `main`.

## Estrutura

- `src/lib.rs` — núcleo do agente (toda lógica de domínio mora aqui)
- `src/main.rs` — casca fina de CLI sobre a lib
- `sessions/`, `cache/` — criados em runtime, fora do git
- `CONTEXT.md` — glossário do domínio (Pesquisa, Sessão, Turno, Evidência)
- `docs/adr/` — decisões de arquitetura
- `docs/agents/` — config das engineering skills (issue tracker, domain docs)
- `docs/research/` — pesquisas de apoio

## Issues

GitHub Issues via `gh`. Ver `docs/agents/issue-tracker.md`.
