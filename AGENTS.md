# AGENTS.md

One product: a CLI Web Search agent behind `run_research`. A Harness calls it once and gets back only the Synthesis.

- Write all repo content in English: docs, code, comments, identifiers, commits, issues, PRs. Chat replies follow the session language.
- Gate before every PR: `cargo fmt --all -- --check && cargo clippy --all-targets -q -- -D warnings && cargo test -q`
- Module layout and the dependency rule (`research → web`, `research → llm`, never the reverse; no `trait` without a second real adapter): `docs/adr/0006-web-search-tool-architecture.md`. Read before adding a module, tool, dependency, or public entry point.

## Where things live

- `CONTEXT.md`: glossary of domain terms. Read when naming a concept.
- `docs/adr/`: one file per decision. Read when a change touches a decision, or two sources disagree.
- `docs/harness.md`: the contract a Harness depends on (env, `--json` shape, exit codes). Read when a change is visible to a Harness.
- `docs/agents/domain.md`: how to use the glossary and ADRs. Read when exploring code.
- `docs/agents/issue-tracker.md`: `gh` conventions for issues and PRs. Read when creating, reading, labeling, or closing either.
- `docs/agents/release.md`: versioning and `CHANGELOG.md` shape. Read when cutting a release or editing the changelog.
