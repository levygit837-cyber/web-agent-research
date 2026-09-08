# Vertical Slice with strong shared/ foundation, no traits

Research grows by Mode (`search` today, `deep` later), not by external tech: Obscura is the single engine (subprocess today, ported code tomorrow — same shape, no real 2nd adapter) and the LLM gateway plus file persistence are already pinned in ADRs 0002/0003; we keep one folder per Mode with glued logic and a strong `shared/` (tools, prompts, types, session, obscura, agent_loop), with concrete functions and zero `trait` until real variation appears.

## Considered Options

- Hexagonal with `Engine`/`Llm` ports — isolates external tech behind `trait`; rejected because swapping Obscura is not an option (no 2nd adapter means a hypothetical seam).
- Clean layers (entities/use_cases/adapters) — pure core with Dependency Rule; rejected because it charges type conversion per crossing and 4 places per feature for a prototype with 1 CLI and files.
- Pure Vertical Slice without `shared/` — each Mode with its own glued fetch; rejected because fetch/session/evidence serve all Modes and would duplicate.

## Consequences

- `src/shared/` holds `agent_loop/` (multi-turn runner every Mode uses), `obscura/` (browser engine), `tools/` (`search`, `fetch` only), `prompts/` (`classify`, `execute`), `types/` (contracts), `session.rs` (Session/Turn/Evidence + JSONL; becomes a folder when `jsonl.rs`/`store.rs` land). `schemas/` was removed until the first real schema exists. `shared/` modules may consume each other; slices consume `shared/`, never siblings.
- Catalog vs 2+ rule: `tools/`, `prompts/` and `types/` are context catalogs (born in `shared/` even with a single consumer); the rest of `shared/` follows the 2+ rule (moves in only after serving a second Mode).
- `src/slices/<mode>/handler.rs` holds one Mode end to end (`search` now, `deep` later). Scoring and caching are behaviors inside tools, not tools; LLM is not a tool — it drives `agent_loop/` via the ADR-0003 gateway.
- Concrete by default; `trait` only if a real 2nd adapter appears (compiler-guided extraction).
- Reevaluate Clean only if external adapters double (e.g. 5 front-ends + 3 stores).
