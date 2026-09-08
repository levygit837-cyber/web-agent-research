# PLAN.md — Issue #11: OpenAI-compatible gateway client

## 0. Scope

Build the minimal HTTP gateway client that lets `shared/agent_loop/` (issue #12)
call the local OpenAI-compatible gateway at `http://localhost:8317/v1`.
Hand-rolled `reqwest` + `serde`, no SDK (ADR-0003). Concrete functions, zero
`trait` (ADR-0004). The LLM is not a tool: it drives `agent_loop/` via this
gateway.

Vocabulary follows `CONTEXT.md` (`Research` / `Session` / `Turn` / `Evidence` /
`Mode` / `Synthesis`). This plan introduces no new domain terms; `thinking`
below is gateway wire vocabulary, not a domain concept.

## 1. Module placement

New engine module `src/shared/gateway/`, registered in `src/shared/mod.rs`
with `pub mod gateway;`.

### 1.1 Layout

```text
src/shared/gateway/
├── mod.rs      # Interface: re-exports + one-paragraph contract
├── config.rs   # GatewayConfig, ModelParams, ResolvedParams, from_env
├── client.rs   # Gateway struct, GatewayError, chat + retry
└── reply.rs    # Wire serde structs, LlmReply, TokenUsage, parse_reply
```

| File        | Contains | Lines (budget) |
|-------------|----------|----------------|
| `mod.rs`    | `pub use` of `Gateway`, `GatewayConfig`, `ModelParams`, `ResolvedParams`, `GatewayError`, `ChatMessage`, `LlmReply`, `TokenUsage`; interface docs | ~30 |
| `config.rs` | Config structs, `from_env`, `effective_params`, `with_model_override` | ~150 + tests |
| `client.rs` | `Gateway::new/with_client/chat`, `backoff_delay`, status mapping, retry loop | ~200 + tests |
| `reply.rs`  | `ChatRequest`, `ChatResponse` family, `parse_reply` | ~200 + tests |

### 1.2 Justification (codebase-design + ADR-0004)

- **Engine, not catalog.** ADR-0004 splits `shared/` into context catalogs
  (`tools/`, `prompts/`, `types/` — born in `shared/` even with one consumer)
  and engines (`obscura/`, `agent_loop/` — one folder per external capability
  every `Mode` consumes). The gateway is an engine exactly like `obscura/`:
  every `Mode` reaches it through `agent_loop/`, never directly. So it lives
  in `shared/` directly, not in a slice and not in `types/`.
- **Catalog vs 2+ rule.** The 2+ rule (move to `shared/` only after serving a
  second `Mode`) applies to slice-born helpers. Engines are exempt the same
  way `obscura/` and `agent_loop/` already are: they exist in `shared/` from
  day one because all slices consume them via `agent_loop/`.
- **One adapter means a hypothetical seam.** There is exactly one gateway
  (the OpenAI-compatible endpoint). Per ADR-0004 and the codebase-design
  "two adapters" rule: no `trait`, no `LlmPort`, no `dyn`. A second
  provider-shaped variation (streaming, a non-compatible API) is the trigger
  for extracting a seam, compiler-guided, at that time.
- **Depth.** External seam is three items: `Gateway::new`,
  `Gateway::chat`, `GatewayConfig::from_env`. Retry, backoff, both thinking
  shapes, and status mapping hide behind `chat`. Internal seams
  (`parse_reply`, `backoff_delay`, `with_client`) are private to the module
  and exist only as the unit-test surface. The deletion test passes: delete
  `gateway/` and retry/thinking-tolerance logic reappears in every caller.

## 2. Config (`config.rs`)

```rust
use std::collections::HashMap;
use std::time::Duration;

/// Per-model parameter overrides. All fields optional; unset falls back
/// to the gateway-level default.
#[derive(Debug, Clone, Default)]
pub struct ModelParams {
    /// Passthrough reasoning-effort string, e.g. "high", "low".
    pub reasoning_effort: Option<String>,
    /// Token cap sent as `max_completion_tokens`.
    pub max_tokens: Option<u32>,
}

/// Fully resolved parameters for one request.
#[derive(Debug, Clone)]
pub struct ResolvedParams {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Base URL, e.g. "http://localhost:8317/v1". No trailing slash assumed;
    /// the client trims one trailing `/`.
    pub base_url: String,
    /// Bearer token. Never logged; `Debug` redacts it (manual impl).
    api_key: String,
    /// Default model id used when the caller does not pick one.
    pub model: String,
    /// Gateway-wide defaults; per-model entries in `model_overrides` win.
    pub default_reasoning_effort: Option<String>,
    pub default_max_tokens: Option<u32>,
    /// model id -> overrides. Precedence: override > default > omitted.
    pub model_overrides: HashMap<String, ModelParams>,
    /// Per-attempt request timeout.
    pub request_timeout: Duration,
    /// Max total attempts including the first try.
    pub max_attempts: u32,
}
```

Constructors (all plain associated functions, no traits):

```rust
impl GatewayConfig {
    pub fn new(base_url: String, api_key: String, model: String) -> Self;
    pub fn from_env() -> anyhow::Result<Self>;
    pub fn with_model_override(mut self, model: &str, params: ModelParams) -> Self;
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self;
    pub fn with_max_attempts(mut self, n: u32) -> Self;
    pub fn effective_params(&self, model: &str) -> ResolvedParams;
}
```

### 2.1 Environment variables and defaults

| Env var                   | Default                       | Notes |
|---------------------------|-------------------------------|-------|
| `GATEWAY_BASE_URL`        | `http://localhost:8317/v1`    | Local dev default |
| `GATEWAY_API_KEY`         | *(none — required)*           | `from_env` errors when missing/empty |
| `GATEWAY_MODEL`           | `glm-5p2`                     | Responds on the live gateway (see §7) |
| `GATEWAY_REASONING_EFFORT`| *(none — omitted)*            | e.g. `high`; passthrough string, never validated client-side |
| `GATEWAY_MAX_TOKENS`      | *(none — omitted)*            | Parsed as `u32`; invalid value is a `from_env` error |
| `GATEWAY_TIMEOUT_SECS`    | `60`                          | Per-attempt timeout |
| `GATEWAY_MAX_ATTEMPTS`    | `3`                           | Total attempts incl. first try |

`from_env` trims one trailing `/` from `base_url`. Missing/empty
`GATEWAY_API_KEY` returns `anyhow::anyhow!("GATEWAY_API_KEY is not set")`
before any I/O (maps to `GatewayError::MissingApiKey` at the call site —
see §5; config itself stays `anyhow` because it runs at startup, not in the
request path).

Per-model overrides are code-level only (`with_model_override`), not env:
an env encoding (`GATEWAY_MODEL_<NAME>_…`) invents a naming scheme the
agent loop (issue #12) does not consume yet. File-based config is a later
concern. Cut per §8.

### 2.2 Override table and precedence

```rust
let cfg = GatewayConfig::from_env()?
    .with_model_override("glm-5p2", ModelParams {
        reasoning_effort: Some("high".to_string()),
        max_tokens: Some(4096),
    });
cfg.effective_params("glm-5p2")
// -> ResolvedParams { reasoning_effort: Some("high"), max_tokens: Some(4096), .. }
// effective_params("other-model") falls back to the GATEWAY_* defaults.
```

Precedence, per field: `model_overrides[model].field` → `default_*` →
omitted from the JSON body (`skip_serializing_if`). An explicit per-model
`None` does **not** clear a default (no three-state option — cut as
unneeded).

## 3. HTTP contract (`reply.rs` + `client.rs`)

`POST {base_url}/chat/completions`, headers `Authorization: Bearer <key>`
and `Content-Type: application/json`. Non-streaming only.

### 3.1 Request structs

```rust
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,    // "system" | "user" (agent loop, issue #12, owns content)
    pub content: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
}

/// One callable function offered to the model. `parameters` is a real JSON
/// Schema value and is always sent.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Serializes to `"auto"` / `"none"` / `{type: "function", function: {name}}`.
/// `ToolDef` serializes to `{type: "function", function: {name, description,
/// parameters}}`. Both fields are omitted from `ChatRequest` when `None`,
/// so `chat()` without tools is byte-compatible with before.
#[derive(Debug, Clone)]
pub enum ToolChoice {
    Auto,
    None,
    Named(String),
}
```

Decisions:

- `max_completion_tokens`, not `max_tokens`: reasoning models served by this
  gateway reject legacy `max_tokens`. When unset, the field is omitted and
  the gateway default applies.
- `reasoning_effort` is an opaque passthrough string, never validated or
  enum-constrained: the gateway forwards vendor-specific values (`high`,
  `low`, …) and an enum would break on the next model.
- No `temperature` / `top_p`: reasoning models reject non-default sampling
  params; the agent loop does not need them.
- No `stream` field: non-streaming is the endpoint default; streaming is an
  explicit non-goal (§8).

### 3.2 Response structs (tolerant to both thinking shapes)

Both shapes below were observed live on this gateway and MUST parse:

- (a) `message.reasoning_content: string` (glm-5p2 style)
- (b) `message.reasoning: string` + `message.reasoning_details: array`
  (mimo/opencode style)

```rust
#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ResponseUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    role: Option<String>,
    // Nullable content: null -> None -> "" (see §4).
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    // Shape (a): glm-5p2 style.
    #[serde(default)]
    reasoning_content: Option<String>,
    // Shape (b): mimo/opencode style scalar …
    #[serde(default)]
    reasoning: Option<String>,
    // … plus detail blocks.
    #[serde(default)]
    reasoning_details: Vec<ReasoningDetail>,
    // Present on the wire; denied, never executed (see §4).
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Default, Deserialize)]
struct ReasoningDetail {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    // Unknown fields ignored by serde default behavior; no flatten catch-all
    // (keeps the struct honest about what we consume).
}

#[derive(Debug, Default, Deserialize)]
struct ToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    function: Option<ToolFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct ToolFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ResponseUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    completion_tokens_details: Option<CompletionDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}
```

Tolerance rules:

- Every field carries `#[serde(default)]`; unknown wire fields are ignored
  (serde default). A new thinking shape degrades to empty `thinking`, never
  to a parse failure.
- `choices: Vec` defaults to empty; empty choices is a semantic error
  (`GatewayError::EmptyChoices`), not a serde error — so the error message
  names the real problem.
- `content` is `Option<String>`: some models emit `"content": null` with
  `refusal` or with thinking-only turns.

## 4. Parsed output (`reply.rs`)

The only type the agent loop (issue #12) consumes:

```rust
/// One completed (non-streaming) turn from the gateway.
#[derive(Debug, Clone, Default)]
pub struct LlmReply {
    /// Concatenated thinking, per precedence in `parse_reply`. May be empty.
    pub thinking: String,
    /// Assistant message text. Never None: null content becomes "".
    pub output: String,
    /// Wire `finish_reason` verbatim ("stop", "length", "tool_calls", …). None if absent.
    pub finish_reason: Option<String>,
    /// Requested tool calls, in wire order. Empty when the model answered
    /// with text; existing text-only consumers ignore this field.
    pub tool_calls: Vec<RequestedToolCall>,
    pub usage: TokenUsage,
}

/// One function call requested by the model. The gateway never executes it;
/// `arguments` is the raw JSON string; `parsed_arguments()` parses it on
/// demand (invalid JSON → `GatewayError::Parse`, non-retryable).
#[derive(Debug, Clone, Default)]
pub struct RequestedToolCall {
    /// Wire `id` (`""` when absent).
    pub id: String,
    /// Requested function name (`""` when absent, but still returned).
    pub name: String,
    /// Raw arguments JSON string (`"{}"` when absent).
    pub arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub reasoning_tokens: u64, // from completion_tokens_details; 0 if absent
}

pub fn parse_reply(resp: ChatResponse) -> Result<LlmReply, GatewayError>;
```

(`ChatResponse` is `pub(crate)`; `parse_reply` is `pub(crate)` — the public
surface is `LlmReply` + `TokenUsage` + `RequestedToolCall` (+ `ToolDef` /
`ToolChoice` / `ChatMessage` for the request side). `ChatMessage` is `pub`
because the agent loop constructs it.)


Parsing rules, in order:

1. `choices` empty → `Err(GatewayError::EmptyChoices)`. Only `choices[0]`
   is used (we never send `n > 1`).
2. `message.refusal` non-empty → `Err(GatewayError::Refused(text))`, even
   if `content` is also present AND even when `tool_calls` are present.
   A refusal is a policy outcome, not output.
3. `message.tool_calls` → collected into `LlmReply::tool_calls` in wire
   order as `RequestedToolCall { id, name, arguments }`: missing `id` → `""`,
   missing `name` → `""` (but still returned), missing
   `function`/`arguments` → `"{}"`. Execution stays in the agent loop
   (issue #12); the gateway never executes. Empty `tool_calls` → empty vec,
   so existing text-only (`stop`) consumers are unaffected.
4. `thinking` = concatenation, in this precedence order, of each non-empty
   part joined with `"\n"`: `reasoning_content` (a), then `reasoning` (b
   scalar), then each `reasoning_details[i].text` (b blocks, in array
   order). Empty parts skipped; no trailing newline. Rationale: when a
   gateway emits both shapes for one turn they describe the same thinking,
   and order (a)→(b) matches most-specific-string first, blocks last.
5. `output` = `content.unwrap_or_default()`. Null content with a
   `finish_reason` of `"stop"` (or `"tool_calls"` with `content: ""`) is a
   legal empty answer, not an error.
6. `usage` = response usage or zeros when absent; `reasoning_tokens` from
   nested `completion_tokens_details`, else 0.

## 5. Errors and retry (`client.rs`)

No `thiserror` dependency (ADR-0003 pins the dep set): manual
`Display` + `std::error::Error` impls (~30 lines, zero new deps).

```rust
#[derive(Debug)]
pub enum GatewayError {
    MissingApiKey,
    Auth,                                        // 401/403: bad key, do NOT retry
    RateLimited { retry_after_secs: Option<u64> }, // 429
    Server { status: u16 },                      // 5xx
    Client { status: u16 },                      // other 4xx (400, 404, 422 …)
    Timeout,                                     // per-attempt deadline
    Transport(String),                           // connection/DNS/TLS
    Parse(String),                               // serde_json failure on 2xx body + invalid tool arguments
    EmptyChoices,                                // 2xx but choices: []
    Refused(String),                             // model refusal text
}
```

Status mapping (checked before any body parse):

| Condition | Variant | Retryable |
|-----------|---------|-----------|
| missing/empty key (pre-flight, no I/O) | `MissingApiKey` | no |
| 401 or 403 | `Auth` | **no** — retrying a bad key burns budget |
| 429, `Retry-After` parsed when present | `RateLimited` | **yes** |
| 5xx | `Server` | **yes** |
| other 4xx | `Client` | no |
| `reqwest` error with `is_timeout()` | `Timeout` | **yes** |
| other `reqwest` error (connect/DNS/TLS) | `Transport` | **yes** (transient network) |
| 2xx body fails serde | `Parse` | no (deterministic) |
| 2xx, `choices: []` | `EmptyChoices` | no |
| `refusal` present (even with `tool_calls`) | `Refused` | no |
| `tool_calls[].parsed_arguments()` invalid JSON | `Parse` | no |

### 5.1 Retry/timeout policy (numbers)

```rust
pub struct Gateway { config: GatewayConfig, client: reqwest::Client }

impl Gateway {
    pub fn new(config: GatewayConfig) -> Self;                    // builds client with config.request_timeout
    pub fn with_client(config: GatewayConfig, client: reqwest::Client) -> Self; // test seam
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmReply, GatewayError>;
    pub async fn chat_with_tools(&self, messages: &[ChatMessage], tools: &[ToolDef], tool_choice: Option<ToolChoice>) -> Result<LlmReply, GatewayError>;
}

pub(crate) fn backoff_delay(attempt: u32, retry_after_secs: Option<u64>) -> Duration;
```

- **Per-attempt timeout: 60 s** (`GATEWAY_TIMEOUT_SECS`, reqwest-level).
  Justification: thinking models routinely take 20–50 s per turn on this
  gateway; anything under ~45 s turns slow successes into timeouts. 60 s
  bounds the worst case while fitting comfortably above observed latency.
- **Max attempts: 3** total (`GATEWAY_MAX_ATTEMPTS`), i.e. 1 try + 2 retries,
  only for retryable variants. Worst-case wall time ≈ 3×60 s + backoff ≈
  ~3 min — bounded and acceptable for a CLI research turn.
- **Backoff: exponential, 1 s × 2^(attempt-1), cap 8 s** — sleeps ≈ 1 s,
  2 s for attempts 1, 2. Fixed backoff hammers a struggling gateway;
  unbounded exponential overshoots a CLI budget. No jitter: deterministic
  delays keep unit tests exact without clock injection.
- **`Retry-After` honored** when present on 429 (seconds value, or HTTP
  date parsed best-effort else ignored), capped at 60 s so one header
  cannot stall a turn past one timeout window. Takes precedence over the
  computed backoff for that sleep.
- Non-retryable errors return immediately on the first attempt.

`tokio::time::sleep` needs the `time` feature, which the pinned `tokio`
dep does **not** enable — the implementer MUST use
`reqwest::ClientBuilder::timeout` (works without it) for the deadline and
must NOT add `tokio/time` features in this issue. Backoff sleep without
`tokio::time`: use `std::thread::sleep`? NO — that blocks the runtime.
Resolution: the retry sleep goes through `tokio::time::sleep`, which
requires enabling `tokio` feature `time`. This is a minimal, additive
feature flip on an already-pinned dep (not a new crate), and the plan
explicitly authorizes exactly this: add `"time"` to the existing tokio
features in `Cargo.toml`. No other dep changes.

## 6. What the agent loop consumes (need validation)

Issue #12 needs exactly: `Gateway::new`, `Gateway::chat(&[ChatMessage]) ->
LlmReply { thinking, output, finish_reason, usage }`,
`Gateway::chat_with_tools(&[ChatMessage], &[ToolDef], Option<ToolChoice>) ->
LlmReply { thinking, output, finish_reason, tool_calls, usage }` (tool
schemas offered per turn; execution stays in `agent_loop/`),
`GatewayConfig::from_env`. Everything else (wire `ToolCall` structs,
`ResolvedParams`, `backoff_delay`, per-status variants) exists to serve
that interface with locality. Cut from this issue: message-history
builders, prompt templates (live in `prompts/`), role enums, multi-choice
handling, usage-based budget accounting (a behavior inside `agent_loop/`,
not a gateway type).

## 7. Test list

Unit tests live inline (`#[cfg(test)] mod tests`) in their module;
the live test is `tests/gateway_live.rs`. No new dev-deps.

1. `reply::tests::shape_a_reasoning_content` — fixture with
   `reasoning_content: "plan…"` (+ null `reasoning_details`) parses to
   `thinking == "plan…"`, `output` from `content`.
2. `reply::tests::shape_b_reasoning_blocks` — fixture with `reasoning`
   scalar + two `reasoning_details` texts parses to all parts joined with
   `"\n"` in order (scalar first, blocks in array order).
3. `reply::tests::both_shapes_concatenate` — (a) + (b) in one message
   concatenate in precedence order §4.4, empties skipped.
4. `reply::tests::null_content_is_empty_output` — `"content": null`,
   `finish_reason: "stop"` → `output == ""`, no error.
5. `reply::tests::refusal_wins` — `refusal` + `content` → `Err(Refused)`.
6. `reply::tests::tool_calls_returned` — exact live shape (`content: ""`,
   `reasoning_content` present, `finish_reason: "tool_calls"`,
   `id`/`type`/`function`/`arguments`) → `Ok` with 1 call asserting
   `id` + `name` + `arguments` + `parsed_arguments()` JSON.
6b. `reply::tests::refusal_wins_over_tool_calls` — `refusal` + `tool_calls`
   → `Err(Refused)`.
6c. `reply::tests::empty_tool_calls_is_text_answer` — absent `tool_calls` →
   empty vec (existing `stop` path unaffected).
6d. `reply::tests::invalid_tool_arguments_is_parse_error` — non-JSON
   `arguments` → `parsed_arguments()` is `Err(Parse)`.
6e. `reply::tests::missing_tool_fields_default` — missing `id` → `""`,
   missing `name` → `""` (still returned), missing
   `function`/`arguments` → `"{}"`.
6f. `reply::tests::request_omits_tools_when_none` — `tools`/`tool_choice`
   omitted when `None` (`chat()` byte-compatible with before).
6g. `reply::tests::request_serializes_tools_and_auto_choice` — OpenAI
   `{type: "function", function: {name, description, parameters}}` shape +
   `"auto"` (plus `"none"` check).
6h. `reply::tests::request_serializes_named_choice` — named choice is
   `{type: "function", function: {name}}`.
7. `reply::tests::empty_choices` — `choices: []` → `Err(EmptyChoices)`.
8. `reply::tests::usage_nesting` — `completion_tokens_details.reasoning_tokens`
   lands in `TokenUsage::reasoning_tokens`; absent `usage` → all zeros.
9. `config::tests::per_model_beats_default` — default
   `reasoning_effort: Some("low")`, override for `glm-5p2` to `"high"`:
   `effective_params("glm-5p2").reasoning_effort == Some("high")`,
   `effective_params("other").reasoning_effort == Some("low")`.
10. `config::tests::unset_means_omitted` — no defaults, no overrides →
    serialized `ChatRequest` JSON contains neither `reasoning_effort` nor
    `max_completion_tokens`.
11. `client::tests::status_mapping` — 401→`Auth` (non-retryable),
    429→`RateLimited` (retryable, `Retry-After: 7` parsed), 500→`Server`
    (retryable), 404→`Client` (non-retryable). Implement with
    `Gateway::with_client` against a local test HTTP server. `reqwest`
    has no test server; use `tokio::net::TcpListener` (needs tokio
    `net` feature — NOT authorized; instead bind via `std::net::TcpListener`
    + `std::thread` serving canned responses, keeping the dep set intact).
12. `client::tests::retry_then_success` — first attempt 429, second 200 →
    one `LlmReply`, two hits observed server-side.
13. `client::tests::no_retry_on_auth` — 401 → exactly one server hit,
    `Err(Auth)`.
13b. `client::tests::chat_with_tools_returns_tool_calls` — 200 tool_calls
    body via `chat_with_tools` returns the `RequestedToolCall` + 1 hit.
13c. `client::tests::chat_with_tools_auth_does_not_retry` — 401 via
    `chat_with_tools` still `Auth`, no retry.
14. `client::tests::backoff_shape` — `backoff_delay(1, None) == 1 s`,
    `backoff_delay(2, None) == 2 s`, `backoff_delay(_, Some(30)) == 30 s`,
    cap respected above 8 s.
15. `tests/gateway_live.rs::live_round_trip` — env-gated: returns early
    `Ok(())` unless `GATEWAY_LIVE=1`, so CI without a gateway stays green.
    Requires `GATEWAY_API_KEY`. Model order: `GATEWAY_LIVE_MODEL` if set,
    else `glm-5p2`, falling back to `mimo-v2.5-free` on 429/`RateLimited`
    (`glm-5.3-flash` is currently 429-limited — do NOT probe it first).
    Sends one user message (`"Reply with exactly: gateway ok"`), asserts
    `output` contains `gateway ok`; asserts `finish_reason` is `Some`;
    `thinking` may be empty (model-dependent) and is only logged, never
    asserted.
16. `tests/gateway_live.rs::live_tools_round_trip` — env-gated like 15.
    Sends `tools=[get_time]` + `tool_choice: auto` (exactly one attempt per
    model, never `glm-5.3-flash` first), asserts at least one tool call named
    `get_time` with parseable arguments; 429 `RateLimited` tries the next
    model, all-rate-limited is `Err`.

## 8. Explicit non-goals

- Streaming / SSE (`stream: true`, event parsing, partial deltas).
- Remote-provider SDKs (`async-openai`, `rig`, vendor crates).
- Tool execution and second-turn synthesis: `chat_with_tools` only offers
  `tools`/`tool_choice` and returns first-class `tool_calls` on `LlmReply`;
  execution stays in the agent loop (issue #12).
- Second adapter / `trait` extraction (no `LlmPort`, no `dyn`, no fakes
  beyond `with_client`).
- Sampling params (`temperature`, `top_p`), multi-choice (`n > 1`),
  file-based config, telemetry/metrics, prompt content (owned by
  `prompts/` + issue #12).

## 9. Implementation order (for the Implementer)

1. `reply.rs` wire structs + `parse_reply` (tests 1–8 drive it).
2. `config.rs` structs + `from_env` + precedence (tests 9–10).
3. `client.rs` error enum + `Gateway::chat` + retry (tests 11–14).
4. `mod.rs` re-exports + `pub mod gateway;` in `src/shared/mod.rs` +
   tokio `time` feature flip in `Cargo.toml`.
5. `tests/gateway_live.rs` (test 15), run once with `GATEWAY_LIVE=1`
   against the local gateway.
6. No changes to `agent_loop/`, slices, prompts, or tools in this issue.
