# 0003 — Runtime: Obscura, Tokio, Axum, persistência

## Fontes

- Tokio `process` (Command/Child, kill_on_drop, zombie/reap): https://docs.rs/tokio/latest/tokio/process/index.html
- Tokio `Command::kill_on_drop`: https://docs.rs/tokio/latest/tokio/process/struct.Command.html
- Tokio `time::timeout` (cancela future, drop cancela): https://docs.rs/tokio/latest/tokio/time/fn.timeout.html
- Tokio `runtime` (multi-thread default/work-stealing, `#[tokio::main]`, enable_io/time): https://docs.rs/tokio/latest/tokio/runtime/index.html
- Axum (Router/handlers, `State(Arc<AppState>)`, tower middleware): https://docs.rs/axum/latest/axum/
- Cargo package layout (`src/lib.rs` + `src/main.rs` + `src/bin/`): https://doc.rust-lang.org/cargo/guide/project-layout.html
- SQLite JSON1 (json(), json_extract, json_each, JSONB): https://www.sqlite.org/json1.html
- Engine Obscura / CDP: fonte primária do repo não resolvida nesta sessão (URL tentada retornou 404); claims abaixo marcadas como [INFERENCE] até confirmar contra o README do engine.

## Achados

- `tokio::process::Command` espelha `std::process::Command` com `spawn`/`status`/`output` assíncronos (fonte: tokio process).
- Por padrão, derrubar o handle `Child` NÃO mata o processo filho; `Command::kill_on_drop(true)` muda para matar no drop (fonte: tokio process, Command).
- Runtime deve colher ("reap") zumbis Unix em best-effort; recomendação é não dropar `Child` antes de `await` completo quando cleanup estrito importa (fonte: tokio process).
- `tokio::time::timeout(dur, fut)` retorna `Err(Elapsed)` e cancela a future; cancelar o timeout = dropar a future (fonte: tokio timeout).
- Multi-thread scheduler (work-stealing, 1 worker por core) é o default e o recomendado para a maioria dos apps; requer feature `rt-multi-thread`; `#[tokio::main]` cria o Runtime (fonte: tokio runtime).
- Runtime hand-built vem sem drivers; é preciso `enable_io`/`enable_time` (ou `enable_all`) — sem isso, timers e networking falham/panicam (fonte: tokio runtime + timeout panics).
- Axum é Route→handler com extractors (`Path`, `Query`, `Json`, `State`) e `IntoResponse`; estado compartilhado canônico é `State(Arc<AppState>)` clonado por request, com `FromRef` para substates (fonte: axum).
- Axum não tem middleware próprio: usa `tower::Service` — timeout, tracing, compression vêm de graça (fonte: axum).
- Cargo: `src/lib.rs` + `src/main.rs` no mesmo package = lib + binário default; binários extras em `src/bin/` (fonte: Cargo layout).
- SQLite armazena JSON como TEXT, com funções JSON1 built-in desde 3.38 e formato binário JSONB desde 3.45; `json_each`/`json_tree` decompõem documentos (fonte: SQLite JSON1).
- [INFERENCE — sem fonte primária confirmada] Subprocesso `obscura fetch` por Turno paga spawn+TLS+handshake a cada chamada; sessão CDP persistente amortiza isso mas exige lifecycle (conectar, re-conectar, isolar contextos). Decisão protótipo: começar por subprocesso com `kill_on_drop(true)` + `timeout`, evoluir para CDP persistente só se a latência medida justificar.
- [INFERENCE — padrão da std/Tokio] JSONL append-only (uma sessão = um `.jsonl`, um evento por linha) é migrável para SQLite porque cada linha já é um objeto JSON válido → `json_each`/importação direta; SQLite adiciona índice e query sem mudar o formato do evento.

## Implicação p/ web-agent-research

- Protótipo: `src/lib.rs` com `Engine::fetch`, `Pipeline::run_turn`, `Session::append`; `src/main.rs` fino (parse args → chama lib); rota Axum posterior (`POST /turn`) chama a mesma lib com `State(Arc<AppState>)` — zero rewrite da lógica.
- Runtime: `#[tokio::main]` (multi-thread default); cada `fetch` via `tokio::process::Command` com `.kill_on_drop(true)` e `tokio::time::timeout`; em `Elapsed`/cancelamento, matar + reap antes de retornar erro do Turno — nunca vazar processo nem zumbi.
- Persistência v1: sessões em `sessions/<id>.jsonl` + cache HTTP em disco (`cache/<hash>.json` com URL, timestamp, TTL); migração p/ SQLite quando for preciso listar/buscar — linhas JSONL entram como estão via JSON1.
