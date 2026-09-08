# PLAN: Issue #14 — Obscura fetch integration (subprocess markdown extractor + fetch tool)

Design-only plan. No implementation here; the Implementer builds it under TDD.

## 0. Ground truth and constraints

- **Issue #14 question**: what subprocess integration (`obscura fetch <url> --dump
  markdown --quiet` extractor over engine URLs, fetch tool wired for the agent
  loop, mandatory tool schema) turns search URLs into Evidence markdown?
- **Ground truth**: revised against the upstream Obscura docs (default CLI
  pipeline, fetch flags and wait levels, markdown extraction; stealth/CDP docs
  for defined error fallback only — no serve/CDP driving). Verified facts used
  below: canonical pipeline `obscura fetch <url> --dump markdown --quiet`
  (URL positional first, flags after; `--quiet` keeps stdout payload-only);
  CLI `--wait-until` defaults to `load`; `--timeout` defaults to 30 s;
  omitting `--wait` uses adaptive settle (5 s cap); batch `scrape` (default
  10 pages, `--concurrency`, needs `obscura-worker` in `PATH`) is a later
  optimization, not this issue. Upstream defines no exit-code/stderr contract
  for blocked fetches, so the conservative marker list (SPEC §3) stands.
  Also grounds on: the issue body, ADR-0004, CONTEXT.md vocabulary, and the
  existing stubs (`src/shared/obscura/*`, `src/shared/tools/*`,
  `src/shared/session.rs`).
- **Binary absent**: `obscura` is not installed in this environment. The test strategy is
  therefore hermetic-first (injectable command double) with exactly one binary-gated
  live check.
- **Non-goals**: `serve`/CDP driving, search engine work (#13), agent loop runner (#12).
  No `src/slices/` or `src/shared/agent_loop/` changes in this issue.
- **Fixed argv, no escalation params**: `fetch_markdown` takes only the URL and
  relies on CLI defaults (`load`, CLI-internal 30 s navigation ceiling,
  adaptive settle). The 60 s engine timeout stays above that ceiling + settle
  (mirroring `scrape`'s 60 s per-URL precedent). The upstream ladder —
  empty SPA shell → `--wait-until load`; nav noise → `--selector "main"`;
  blocked → `--stealth` retry, then `--proxy` + timezone/geolocation; login
  wall → `serve` + CDP; many URLs → batch `scrape` (default 10 pages,
  `--concurrency`, needs `obscura-worker` in `PATH`) — is future work for
  later issues, not caller-configurable flags here. `scrape` batching in
  particular stays out: one subprocess per URL in this issue.
- **ADR-0004 rules** (binding): no `trait`/`dyn` until a real 2nd adapter appears —
  concrete functions and structs only; `shared/obscura` is the single engine;
  `shared/` modules may consume each other, slices consume `shared/` never siblings;
  the tools catalog holds executable behavior only (`search`, `fetch`).
- **CONTEXT.md language** (binding): the unit produced is **Evidence** — web-extracted
  content with source URL and collection time, used in synthesis. Never call it
  source/document/snippet/chunk in code or docs.

## 1. Seam placement (deep-module vocabulary)

Two modules, one seam each:

| Module | Interface (the seam) | Depth |
|---|---|---|
| `shared/obscura` (engine) | `Obscura::fetch_markdown(url) -> Result<FetchedMarkdown, FetchError>` | **Deep**: subprocess spawn, argv shape, timeout + kill, stdout/stderr classification, URL validation — all hidden behind one async fn. Deletion test passes: without it, every caller re-implements process management. |
| `shared/tools/fetch` (tool) | `fetch_tool(input, engine) -> Result<Evidence, FetchError>` + `fetch_tool_schema()` | **Deliberately shallow**: validates tool JSON, delegates to the engine, wraps markdown in Evidence. Its leverage comes from the schema contract, not hidden logic. |

Decisions from this framing:

1. **URL validation lives in `shared/obscura`, not in the tool.** The extractor's
   interface promises "give me a URL, get markdown"; rejecting garbage is part of that
   contract, so every future caller (`search` slice today, `deep` slice tomorrow) gets
   it for free (locality). The tool just forwards `FetchError::InvalidUrl`.
2. **Testability without traits**: the internal seam is a concrete struct, not a seam in
   the public interface. `Obscura { binary: PathBuf, timeout: Duration }` — tests point
   `binary` at a fixture shell script. No trait, no `dyn`, no second adapter: ADR-0004
   compliant. The public interface stays one method; the constructor parameter is the
   internal seam used by tests (cf. codebase-design: internal vs external seams).
3. **No new `src/` modules.** The stubs already name the right homes: extractor logic
   goes in `src/shared/obscura/browser.rs` ("browser functions backing search and
   fetch"), Evidence goes in `src/shared/session.rs` (its docstring already claims
   "Session, Turn and Evidence"). New files are only `tests/fixtures/obscura-fake.sh`
   and integration tests. Boring is good.
4. **No new dependencies.** URL validation uses `reqwest::Url` (reqwest is already a
   dep). Timestamps use `std::time::SystemTime` (no chrono). Errors are a hand-rolled
   enum (no thiserror). The one manifest change: add tokio `"process"` and `"time"`
   features (current manifest has only `rt-multi-thread` + `macros`, so neither
   `tokio::process::Command` nor `tokio::time::timeout` compiles today).

## 2. Stage breakdown

### Stage 0 — Evidence contract (`src/shared/session.rs`)

- Add the `Evidence` struct per SPEC §4 (serde Serialize/Deserialize, `new()` stamps
  collection time). No behavior, no I/O.
- Done when: struct round-trips through `serde_json` (covered by Stage 3 tests).

### Stage 1 — Subprocess extractor (`src/shared/obscura/browser.rs`)

- Add `Obscura` struct + `fetch_markdown` + `FetchedMarkdown` + `FetchError` per
  SPEC §1–§3.
- Fixed argv shape: `obscura fetch <normalized-url> --dump markdown --quiet`
  (URL positional first, flags after — the upstream default pipeline order;
  `--quiet` keeps stdout payload-only). Stdout = markdown, stderr =
  diagnostics only. No stdin. Relied-upon CLI defaults, not passed explicitly:
  `--wait-until load`, adaptive settle when `--wait` is omitted (5 s cap).
  Spawn with `kill_on_drop(true)`, then `tokio::time::timeout` around
  `child.wait_with_output()`; on expiry the dropped future kills the child
  (`child.kill().await` after `timeout` is unimplementable — `wait_with_output`
  owns the `Child`). `Obscura::default` timeout is 60 s: headroom over the
  CLI's own 30 s navigation ceiling + 5 s settle (mirroring `scrape`'s 60 s
  per-URL precedent), so a slow-but-healthy fetch is not misclassified.
- URL validation path (runs **before** any spawn): parse with `reqwest::Url`,
  require absolute URL with `http`/`https` scheme. Anything else →
  `FetchError::InvalidUrl`, no process spawned.
- Classification order on completion (SPEC §3): spawn failure → `CommandFailed`;
  timeout → `Timeout`; stderr blocked-markers → `Blocked` (even on exit 0,
  with boundary-guarded `403`/`bot` matching so `14034`/`robots` chatter never
  matches); non-zero exit → `CommandFailed` with status + stderr tail;
  exit 0 + blank stdout → `EmptyBody`.
- Done when: all Stage 3 hermetic cases pass.

### Stage 2 — Fetch tool + mandatory schema (`src/shared/tools/fetch.rs`)

- Add `FETCH_TOOL_NAME`, `fetch_tool_schema()`, `FetchInput::parse`,
  `fetch_tool` per SPEC §5–§6. The tool constructs `Evidence` via
  `Evidence::new(normalized_url, markdown)` — the tool never invents timestamps
  or rewrites markdown.
- The schema object is returned verbatim from `fetch_tool_schema()` so the agent
  loop (#12) and its tests share one source of truth; `FetchInput::parse`
  accepts exactly what the schema advertises.
- Done when: schema test + parse tests + happy-path tool test (via hermetic
  engine) pass. No `agent_loop/` edits — #12 consumes this interface later.

### Stage 3 — Hermetic harness + the one live check (`tests/`)

- `tests/fixtures/obscura-fake.sh`: executable shell double emulating the CLI
  shape (`fetch <url> --dump markdown --quiet`: dispatches on `$2`, the
  positional URL — never the last argv element, which is `--quiet`). Routes:
  substring `empty` → exit 0, no output; `blocked` → exit 3, stderr
  `access forbidden (bot denied)`; `slow` → sleep 5 then markdown; default →
  exit 0 with `# Title\n\nbody for <url>`.
- `tests/fetch_obscura.rs` (hermetic, always run): engine pointed at the fixture —
  happy path returns trimmed markdown; the four error cases map exactly
  (non-URL input, empty body, blocked, timeout with a short `Obscura` timeout);
  plus schema-shape test and `FetchInput::parse` rejections.
- `tests/live_obscura.rs` (**exactly one** live test, `#[ignore]`):
  `live_obscura_fetch_returns_markdown` builds `Obscura::default()` and fetches
  `https://example.com`; on spawn failure it returns `Ok(())` (binary absent =
  skip, not failure). Run explicitly via `cargo test -- --ignored` where the
  binary exists.
- Done when: `cargo test` is green without the binary; the ignored test is the
  only test that touches a real `obscura`.

### Stage 4 — Error-mapping matrix + handoff

- Verify the full matrix (SPEC §3 table) against the hermetic suite; confirm
  `Display` strings carry url + cause for log readability.
- Confirm handoff seams for later issues without touching them: #12 consumes
  `fetch_tool` + `fetch_tool_schema()`; #13 consumes `Obscura` for its own
  paths. No changes outside `shared/obscura`, `shared/tools`, `shared/session`
  (plus `Cargo.toml` tokio features and `tests/`).

## 3. File-by-file changes (implementer scope)

| File | Change |
|---|---|
| `Cargo.toml` | Add `"process"`, `"time"` to tokio features. Nothing else. |
| `src/shared/session.rs` | Add `Evidence` struct + `new()` (SPEC §4). |
| `src/shared/obscura/browser.rs` | Add `Obscura`, `FetchedMarkdown`, `FetchError` + `Display`/`Error` impls, validation, spawn/timeout/classify (SPEC §1–§3). |
| `src/shared/obscura/mod.rs` | Re-export (`pub use browser::{...}`) if siblings need it; one line. |
| `src/shared/tools/fetch.rs` | Add `FETCH_TOOL_NAME`, `fetch_tool_schema()`, `FetchInput`, `fetch_tool` (SPEC §5–§6). |
| `tests/fixtures/obscura-fake.sh` | New, executable hermetic double (routes above). |
| `tests/fetch_obscura.rs` | New hermetic suite (happy + 4 errors + schema + parse). |
| `tests/live_obscura.rs` | New single `#[ignore]` live check. |

Untouched: `src/slices/**`, `src/shared/agent_loop/**`, `src/shared/tools/search.rs`,
`src/shared/types/**`, `src/main.rs`, `src/lib.rs`.

## 4. Risks
1. **Upstream defines no exit-code/stderr contract for blocked fetches**
  (fetch.md/stealth.md give the retry ladder, not machine-readable errors).
   Mitigation: conservative marker list; anything unrecognized falls into
   `CommandFailed` with the stderr tail preserved, so reclassification later is a
   one-spot change (locality).
2. **Fixture drift from the real CLI.** Mitigation: the single live test pins the
   argv shape and happy path; any drift fails loudly there, not in N hermetic tests.
3. **Timeout-kill races on slow CI.** Mitigation: hermetic timeout test uses a fixture
   sleep comfortably above the test timeout (e.g. sleep 5s vs 200ms timeout).
