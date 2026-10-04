# Releases

Read when cutting a release, bumping the version, editing `CHANGELOG.md`, or writing GitHub release notes.

## Version

- SemVer. Before 1.0, a breaking change bumps minor (`0.3.0` → `0.4.0`); anything else bumps patch.
- Breaking means a Harness-visible change (env default, `--json` shape, exit code meaning) or a public Rust API change.
- Bump `Cargo.toml` and `Cargo.lock` in the same commit.

## Steps

1. Gate `main`: `cargo fmt --all -- --check && cargo clippy --all-targets -q -- -D warnings && cargo test -q`, plus the ignored live tests the current network allows.
2. Branch `release/vX.Y.Z`. Move `[Unreleased]` into `## [X.Y.Z] - YYYY-MM-DD` and update the compare links.
3. PR titled `release: vX.Y.Z`. Body: `## Summary` (the section's summary line) and `## Tests run` (one bullet per check). CI green, then merge.
4. `git tag -a vX.Y.Z -m vX.Y.Z` on the merge commit; push the tag.
5. `gh release create vX.Y.Z --title vX.Y.Z --notes-file <section>`. The notes are the CHANGELOG section, verbatim.

## CHANGELOG section shape

```markdown
## [X.Y.Z] - YYYY-MM-DD

One sentence: what a Harness user gets from this release.

### Breaking
### Added
### Changed
### Fixed
### Removed
### Known issues
```

- Keep this section order; omit empty sections.
- One line per bullet, about 100 characters, no sub-bullets.
- At most 10 bullets per section. Merge related PRs into one bullet: `(#62, #63)`.
- Each bullet says what changed for the user, then the PR numbers. Env vars, commands, paths and exit codes go in backticks.
- Detail (how it works, measurements, rationale) lives in `docs/harness.md`, the ADRs, `docs/research/` and the PRs.
- Internal refactors and test-only changes get no entry. A refactor that moves a public path gets one Breaking bullet.
- Each PR adds its bullets to `[Unreleased]` in this shape.
