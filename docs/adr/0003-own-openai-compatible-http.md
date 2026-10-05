# No LLM SDK: our own OpenAI-compatible HTTP client

The agent must call any provider (OpenAI, OpenRouter, Ollama, vLLM) without SDK lock-in; we decided on hand-written `reqwest` + `serde` against an OpenAI-compatible endpoint, accepting that retries, streaming, and structured output are hand-rolled.

## Considered Options

- A dedicated SDK (`async-openai`, `rig`) — less initial code; the cost is version coupling and lag in adopting a local provider.
- Local-first (Ollama) — no cost or key; the cost is deferring validation against a remote provider. Kept as a mode behind the same interface, not as an exclusive decision.

## Consequences

- Anchor dependencies: `tokio` (rt-multi-thread), `reqwest` (rustls-tls, no default features), `serde`/`serde_json`, `clap` (derive), `tracing` + `tracing-subscriber` (env-filter), `anyhow`.
- `SESSION_FORMAT_VERSION` lives in `src/lib.rs`; the JSONL format migrates 1:1 to a future `turns` table.
- Obscura starts as a subprocess (`obscura fetch`) or CDP; a native client is out of scope for the prototype. [ADR-0006](0006-web-search-tool-architecture.md) later made fetch reqwest-first with Obscura as the fallback.
