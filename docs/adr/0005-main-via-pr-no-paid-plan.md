# `main` only via PR, without a paid plan

A private repo on the free plan has no branch protection or rulesets (the API returns 403, verified on 2026-09-07); we decided on a PR-only convention for `main` with post-push detection (the `protect-main` workflow) and a local block (the `pre-push` hook in `.githooks/`), accepting that a direct push cannot be blocked on the server until we move to Pro or make the repo public.

## Considered Options

- Make the repo public — unlocks protection/rulesets for free; the cost is exposing the prototype and its history too early.
- Subscribe to GitHub Pro — unlocks protection on a private repo; the cost is a monthly fee for a solo prototype.
- Convention only, no enforcement — zero code; the cost is relying on discipline, with no signal when it fails.
- Server-side pre-receive hooks — real custom blocking; the cost is requiring GitHub Enterprise, out of reach.

## Consequences

- Every change to `main` enters through a PR with a merge commit (`gh pr create` → `gh pr merge`); a direct push is a violation, even with red CI — an emergency is resolved with a revert PR, never with a push.
- `protect-main.yml` (on push to `main`) fails when any new commit is neither a merge of a PR in the MERGED state nor associated with a merged PR through the commits API; the failure message carries the steps (revert on a new branch + PR, no force-push — rewriting `main` also trips the gate).
- The `.githooks/pre-push` hook blocks a local `push` to `refs/heads/main`; enable it per clone with `git config core.hooksPath .githooks` (a hook is opt-in per clone and cannot be versioned as mandatory).
- CI (`ci.yml`) runs `lint` (fmt + clippy) and `test` (test + `--locked` build) on push/PR to `main`, plus an `msrv` job that checks the declared minimum Rust version; on migration, `lint` + `test` + `no-direct-push` become the required checks, with a PR required, 1 approval, `dismiss stale approvals`, an up-to-date branch, and no force-push or deletion.
- No exemptions: recreating `main` by push fails closed — even the root commit needs an associated PR (the historical bootstrap predates the gate).
