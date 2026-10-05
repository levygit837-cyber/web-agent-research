# Sem SDK de LLM — HTTP OpenAI-compatible próprio

Agente precisa chamar qualquer provedor (OpenAI, OpenRouter, Ollama, vLLM) sem lock-in de SDK; decidimos `reqwest` + `serde` na mão contra endpoint OpenAI-compatible, aceitando hand-roll de retries/streaming/structured-output.

## Considered Options

- SDK dedicado (`async-openai`, `rig`) — menos código inicial; custo é amarra de versão e atraso para adotar provedor local.
- Local-first (Ollama) — zero custo/chave; custo é adiar validação contra provedor remoto. Mantido como modo via mesma interface, não como decisão exclusiva.

## Consequences

- Deps âncora: `tokio` (rt-multi-thread), `reqwest` (rustls-tls, sem default-features), `serde`/`serde_json`, `clap` (derive), `tracing` + `tracing-subscriber` (env-filter), `anyhow`.
- `SESSION_FORMAT_VERSION` em `src/lib.rs`; formato JSONL migrável 1:1 para futura tabela `turns`.
- Obscura primeiro via subprocesso `obscura fetch` / CDP; cliente nativo fora do escopo do protótipo.
