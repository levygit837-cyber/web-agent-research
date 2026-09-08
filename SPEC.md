# SPEC: Issue #14 — Obscura fetch integration

Companion to `PLAN.md`. Exact interface contract the Implementer builds against.
All Rust items live in the stated modules; no `trait`, no `dyn`, no generics
(ADR-0004). All repo content in English; code identifiers in English.

## 0. Shared vocabulary (CONTEXT.md)

- The unit produced is **Evidence**: web-extracted content with source URL and
  collection time, used in synthesis. Code, logs, and docs say `Evidence`,
  `source_url`, `collected_at`, `markdown` — never source/document/snippet/chunk.

## 1. Extractor — `src/shared/obscura/browser.rs`

```rust
use std::path::PathBuf;
use std::time::Duration;

/// Concrete handle to the single Obscura engine (subprocess today).
/// `binary` is the internal seam tests use to inject the fixture double.
pub struct Obscura {
    pub binary: PathBuf,
    pub timeout: Duration,
}

impl Obscura {
    /// Production handle: `obscura` on PATH, 30 s per-fetch timeout.
    pub fn new(binary: PathBuf, timeout: Duration) -> Self;

    /// Spawn `obscura fetch --dump markdown --quiet <normalized-url>`,
    /// collect stdout as markdown, classify failures per §3.
    pub async fn fetch_markdown(&self, raw_url: &str) -> Result<FetchedMarkdown, FetchError>;
}

impl Default for Obscura {
    /// `Obscura { binary: "obscura".into(), timeout: Duration::from_secs(30) }`.
    fn default() -> Self;
}

/// Trimmed markdown plus the normalized URL it was fetched from.
pub struct FetchedMarkdown {
    pub url: String,
    pub markdown: String,
}
```

Rules:

- **Validation first** (before any spawn): parse `raw_url` with `reqwest::Url`,
  require an absolute URL whose scheme is `http` or `https`. Violations return
  `FetchError::InvalidUrl` without spawning a process.
- **Normalization**: the URL passed to the child and stored in
  `FetchedMarkdown::url` is the parsed URL re-serialized (`Url::to_string`),
  so `Evidence.source_url` is canonical.
- **Argv** (exact, in order): `fetch`, `--dump`, `markdown`, `--quiet`,
  `<normalized-url>`. No stdin. Stdout = markdown bytes; stderr = diagnostics.
- **Timeout**: `tokio::time::timeout(self.timeout, child.wait_with_output())`;
  on expiry, kill the child (`child.kill().await`) and return
  `FetchError::Timeout`.
- **Trimming**: `markdown` is `stdout` decoded lossily (`String::from_utf8_lossy`)
  and trimmed. A whitespace-only stdout is empty.

`Cargo.toml` prerequisite (only manifest change in this issue): tokio needs the
`"process"` and `"time"` features for `tokio::process::Command` and
`tokio::time::timeout` (current manifest has only `rt-multi-thread`, `macros`).

## 2. `FetchError` — same module

```rust
use std::fmt;

/// Every failure mode of `fetch_markdown`, and of the fetch tool by delegation.
pub enum FetchError {
    /// Input is not an absolute http(s) URL. No subprocess was spawned.
    /// `input` is the raw caller-supplied string.
    InvalidUrl { input: String },
    /// Exit 0 but stdout was empty/whitespace-only.
    EmptyBody { url: String },
    /// Engine reported access denial (exit code or stderr markers, §3).
    /// `reason` is the trimmed stderr tail (max 500 chars).
    Blocked { url: String, reason: String },
    /// No output within the engine timeout; the child was killed.
    Timeout { url: String, after: Duration },
    /// Spawn failure or any other non-zero exit / I/O error.
    /// `detail` carries the OS error or `status + stderr tail` (max 500 chars).
    CommandFailed { url: String, detail: String },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result;
}

impl std::error::Error for FetchError {}
```

### `Display` mapping (exact strings)

| Variant | Display |
|---|---|
| `InvalidUrl { input }` | `invalid url: {input} (expected absolute http(s) URL)` |
| `EmptyBody { url }` | `empty body: {url} returned no markdown` |
| `Blocked { url, reason }` | `blocked: {url}: {reason}` |
| `Timeout { url, after }` | `timeout: {url} after {after:?}` (e.g. `after 30s`) |
| `CommandFailed { url, detail }` | `command failed: {url}: {detail}` |

## 3. Classification order and error matrix

On process completion, classify in this order (first match wins):

1. Spawn / I/O failure → `CommandFailed`.
2. Timeout elapsed (child killed) → `Timeout`.
3. Stderr contains a blocked marker (case-insensitive substring, checked
   **regardless of exit code**): `403`, `forbidden`, `access denied`,
   `blocked`, `bot`, `captcha`, `rate limit`, `rate-limited`, `too many requests`
   → `Blocked` with the trimmed stderr tail as `reason`.
4. Non-zero exit status → `CommandFailed` with `detail = "exit {status}: {tail}"`.
5. Exit 0, trimmed stdout empty → `EmptyBody`.
6. Otherwise `Ok(FetchedMarkdown { url: normalized, markdown: trimmed })`.

Matrix (all four mandatory error cases):

| Input / fixture route | Expected |
|---|---|
| `not-a-url`, `""`, `ftp://x/y`, `/relative/path` | `InvalidUrl`, no spawn |
| exit 0, empty stdout | `EmptyBody` |
| stderr `access forbidden (bot denied)` (any exit) | `Blocked` |
| sleep 5 s, engine timeout 200 ms | `Timeout` |
| non-zero exit, plain stderr | `CommandFailed` |
| exit 0, `# Title\n\nbody` | `Ok`, markdown trimmed |

## 4. Evidence — `src/shared/session.rs`

```rust
use serde::{Deserialize, Serialize};

/// Web-extracted content with source URL and collection time, used in synthesis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Normalized source URL (from `FetchedMarkdown::url`, never the raw input).
    pub source_url: String,
    /// Collection time, RFC 3339 UTC (stamped by `new` from `SystemTime`; no chrono).
    pub collected_at: String,
    /// Trimmed markdown body (from `FetchedMarkdown::markdown`, stored verbatim).
    pub markdown: String,
}

impl Evidence {
    /// Stamp `collected_at` with the current UTC time.
    pub fn new(source_url: String, markdown: String) -> Self;
}
```

Field rules: `source_url` is always the engine-normalized URL; `collected_at`
is always stamped at construction (callers never pass timestamps);
`markdown` is stored exactly as the engine returned it.

## 5. Fetch tool — `src/shared/tools/fetch.rs`

```rust
use serde_json::Value;
use crate::shared::obscura::browser::{FetchError, Obscura};
use crate::shared::session::Evidence;

/// Tool name registered with the agent loop.
pub const FETCH_TOOL_NAME: &str = "fetch";

/// Caller-supplied tool input: exactly what the §6 schema advertises.
pub struct FetchInput {
    pub url: String,
}

impl FetchInput {
    /// Shape check only: `value` must be an object with a string `url`.
    /// URL *validity* is the engine's job (§1); parse errors surface as
    /// `FetchError::InvalidUrl { input }`.
    pub fn parse(value: &Value) -> Result<Self, FetchError>;
}

/// The mandatory tool-definition object consumed by the agent loop (#12).
/// Single source of truth: loop wiring and tests share this value.
pub fn fetch_tool_schema() -> Value;

/// Validate input, delegate to the engine, wrap markdown in Evidence.
/// `Evidence.source_url` / `.markdown` come from `FetchedMarkdown`;
/// `collected_at` is stamped by `Evidence::new`.
pub async fn fetch_tool(input: &Value, engine: &Obscura) -> Result<Evidence, FetchError>;
```

`FetchInput::parse` rejections: non-object → `InvalidUrl { input: <compact JSON> }`;
missing/non-string `url` → `InvalidUrl { input: <value-or-"" > }`. Extra
properties are ignored by `parse` (schema advertises `additionalProperties: false`;
strictness is enforced at the loop boundary, not here).

## 6. Fetch tool JSON schema (mandatory for the agent loop)

`fetch_tool_schema()` returns **exactly** this object (whitespace-insignificant):

```json
{
  "name": "fetch",
  "description": "Fetch a URL via the Obscura engine and return its content as markdown Evidence for synthesis.",
  "parameters": {
    "type": "object",
    "properties": {
      "url": {
        "type": "string",
        "format": "uri",
        "description": "Absolute http(s) URL to fetch markdown from."
      }
    },
    "required": ["url"],
    "additionalProperties": false
  }
}
```

Conformance: `schema["name"] == FETCH_TOOL_NAME`; `parameters.required == ["url"]`;
`parameters.properties` has exactly one key (`url`); `additionalProperties == false`.
`FetchInput::parse` accepts exactly the inputs this schema describes.

## 7. Out of scope (non-goals)

`serve`/CDP driving, search engine paths (#13), agent loop runner (#12),
`src/slices/**` changes, new dependencies beyond tokio `"process"`/`"time"`.
