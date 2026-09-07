# Rust como linguagem âncora

CLI protótipo que evolui para API multi-turno sobre Obscura exige async estável e overhead mínimo; decidimos Rust + tokio, aceitando iteração mais lenta em troca de binário único e afinidade nativa com Obscura (também Rust, via CDP).

## Considered Options

- TypeScript + Node 22 — Playwright/Puppeteer first-class contra Obscura via CDP sem adapter, Vercel AI SDK/Mastra prontos; custo é runtime Node + dois binários.
- Python 3.12 + FastAPI — maior ecossistema agente/LLM, prototipação mais rápida; custo é GIL, packaging e tipagem mais frágeis.
- Go — concorrência e CLI simples; custo é ecossistema LLM/agente menor e libs CDP menos maduras.

## Consequences

- Loop de ferramentas LLM via HTTP OpenAI-compatible próprio (`reqwest` + `serde`), sem SDK dedicado — evita lock-in, exige hand-roll de retries/streaming/structured-output.
- Integração Obscura primeiro via subprocesso `obscura fetch` / CDP, depois cliente nativo; compilações lentas no loop de protótipo.
