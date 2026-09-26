# One product: the Web Search agent over a stable web layer

Supersedes ADR-0004. Amends ADR-0001 (Obscura is no longer the first fetch path).

The repo drifted into two products: raw web tools (`shared/tools/`, `shared/web_engine_search/`) and a research agent (`shared/agent_loop/`, `slices/search/`) that never called them. `ToolRegistry::live()` returns canned stubs (#25, #26), the registry declares its own schemas and hit/page types that duplicate `shared/tools/` and `shared/types/search.rs`, and `slices/` holds one Mode while `deep` does not exist. We decide there is one product: a **Web Search** tool that a Harness calls through the CLI; it runs one Research and returns only the Synthesis. Search and fetch are internal capabilities of that agent, not public tools.

## Decision

1. **Public interface is one call.** `web_agent_research::run_research(ResearchRequest) -> Result<ResearchResponse, ResearchError>`, surfaced as `web-agent-research research "<goal>" --json`. No raw `search`/`fetch` subcommands, no MCP, no HTTP server.
2. **Modules by capability, strict dependency direction:**

   ```text
   main.rs ─► research ─► web
                     └──► llm
   ```

   - `web/`: search (DDG + Startpage fan-out) and fetch (URL → Evidence). No LLM, no Session. Stable on its own.
   - `llm/`: OpenAI-compatible gateway (ADR-0003). Knows nothing about tools or web.
   - `research/`: the agent. Loop, context, prompts, synthesis, Session JSONL, tool dispatch into `web/`. Only module that knows Research/Turn.
   - `web/` and `llm/` never import `research/` or each other.
3. **Fetch is reqwest-first, Obscura fallback.** `reqwest` GET → HTML→markdown in-process (`htmd`). Fall back to `obscura fetch --dump markdown` only when the static result is unusable: challenge/blocked status (403/429/503 with challenge markers), a JS shell (extracted markdown under a length threshold), or a non-HTML-renderable response. Obscura missing on PATH makes fallback a typed error, never a panic. Measured 2026-09-26 on 3 pages: Obscura 1.5–8.2 s vs plain HTTP 0.3–0.55 s.
4. **One schema, one type per concept.** Each internal tool owns its JSON schema, argument parsing, and result type next to its implementation in `web/`; `research/` dispatches to them and adds no parallel definitions. Search results are Hits (candidates); only fetched content is Evidence.
5. **Mode is a parameter, not a folder.** `search` is the only Mode. `deep` gets a module when it is built, not before.
6. **No test-only behavior in production constructors.** Tests inject doubles through explicit internal seams (base URLs, binary path, a `#[cfg(test)]` tool queue); `live()`-style stubs are banned.

## Considered Options

- Keep ADR-0004 (vertical slices + `shared/`): rejected. With one Mode the slice is a pass-through, and `shared/` became a mixed bag where catalogs, engines, and duplicates coexist without a dependency rule.
- Expose raw `search`/`fetch` alongside Web Search: rejected. Three public contracts to keep stable, and it brings back the two-product split.
- MCP server now: deferred. CLI + `--json` works in every Harness that has a shell; revisit when a Harness needs typed tool discovery.
- Obscura-only fetch: rejected for latency (5–15× slower) and a hard binary dependency. Obscura-free fetch: rejected because SPAs and anti-bot pages would lose content.

## Consequences

- `src/shared/` and `src/slices/` go away. The move is mechanical, done in one PR, with no behavior change, before any feature work.
- `ToolRegistry` stub mode, its local schemas (`query`/`top_k`, `url`/`max_chars`), and `SearchHit`/`FetchedPage` are deleted when the real tools are wired; #25 and #26 close there.
- Empty placeholders (`prompts/classify.rs`, `types/research.rs`) are deleted; classification comes back only with a second Mode.
- New dependency: `htmd` (HTML→markdown). `scraper` stays for search parsing.
- The ADR-0002 on-disk fetch cache stays unbuilt; decide after real tools are wired and repeat fetches are measured.
- `trait` is still banned until a second adapter exists; Obscura is a fallback branch inside `web::fetch`, not an adapter behind a seam.
