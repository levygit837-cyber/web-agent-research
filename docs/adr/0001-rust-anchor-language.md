> **Amended by [ADR-0006](0006-web-search-tool-architecture.md):** fetch is reqwest-first; Obscura is the fallback, not the first path.

# Rust as the anchor language

A prototype CLI that grows into a multi-turn API on top of Obscura needs stable async and minimal overhead; we decided on Rust + tokio, accepting slower iteration in exchange for a single binary and native affinity with Obscura (also Rust, driven over CDP).

## Considered Options

- TypeScript + Node 22 — Playwright/Puppeteer are first-class against Obscura over CDP with no adapter, and the Vercel AI SDK/Mastra are ready-made; the cost is a Node runtime and two binaries.
- Python 3.12 + FastAPI — the largest agent/LLM ecosystem and the fastest prototyping; the cost is the GIL, packaging, and weaker typing.
- Go — simple concurrency and CLIs; the cost is a smaller LLM/agent ecosystem and less mature CDP libraries.

## Consequences

- The LLM tool loop talks to an OpenAI-compatible endpoint over our own HTTP client (`reqwest` + `serde`), with no dedicated SDK — this avoids lock-in and means retries, streaming, and structured output are hand-rolled.
- Obscura integration starts as a subprocess (`obscura fetch`) or CDP, with a native client later; slow compiles are a cost in the prototype loop.
