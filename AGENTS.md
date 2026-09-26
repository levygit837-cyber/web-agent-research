# AGENTS.md

Write all repo content in English: docs, code, comments, identifiers, commits, issues, PRs. Chat replies follow the session language; files stay English.

## Architecture

One product: the Web Search agent behind `run_research`. Read `docs/adr/0006-web-search-tool-architecture.md` before adding a module, a tool, a dependency, or a public entry point; it holds the module layout and the dependency rule (`research → web`, `research → llm`, never the reverse).

## Agent skills

### Issue tracker

GitHub issues hold issues and PRDs, operated via `gh`. Read `docs/agents/issue-tracker.md` when creating, reading, listing, commenting on, labeling, closing, or triaging an issue or PR.

### Domain docs

Single-context repo: `CONTEXT.md` defines the glossary, `docs/adr/` records decisions. Read `docs/agents/domain.md` when exploring code, naming a domain concept, or resolving a decision conflict.
