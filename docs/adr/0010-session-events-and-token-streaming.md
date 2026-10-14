# Session Events, Session v2 log and token streaming

Answers #146 (epic #145). Amends ADR-0002 (the Session file becomes an incremental log) and ADR-0007 (the `Gateway` seam gains one observed call). Keeps ADR-0006's dependency rule and its `trait` ban.

A Session is opaque until it ends. The CLI writes nothing while the agent works, the Session JSONL is written only after a successful answer (a Session that exits 3, 5 or 9 leaves no file), and the gateway always sends `stream: false`. A person cannot watch a Session, and a Harness or auditor cannot see why one failed. We decide on one in-process Event model that feeds every view, an incremental Session log, and SSE token streaming in both wire formats, on by default.

## Decision

### 1. One Event model, three outputs

- `research` owns the Event type and one emitter handle. Every observable step is one Event, emitted synchronously, in order, from `research` code (the loop, tool dispatch, `run_research`). `web/` never emits and never learns about Events.
- Outputs are concrete fields on the emitter, not implementations of a shared interface: the NDJSON writer (`--events`), the Progress renderer (stderr), and the Session log. No `trait`, no channel, no background task: emit writes and flushes each enabled output before it returns, so a crash loses at most the Event being written.
- The public entry point stays `run_research(ResearchRequest)` (ADR-0006). The new knobs ride on `ResearchRequest` as fields with serde defaults; `main.rs` maps CLI flags onto them.

### 2. Envelope

One JSON object per line:

```json
{"v":1,"seq":12,"ts":"2026-10-14T19:40:02.118Z","elapsed_ms":6812,"session_id":"1790000000-4242","turn":3,"type":"fetch.finished","data":{}}
```

- `v`: envelope version, independent of `SESSION_FORMAT_VERSION`. Adding a field or a type keeps `v`; renaming or removing one bumps it.
- `seq`: starts at 1, strictly increasing per Session, no gaps across outputs (every output sees the same `seq`, so an output that filters Events shows the gaps).
- `ts`: UTC RFC 3339 with milliseconds. `elapsed_ms`: since the emitter was created.
- `turn`: the Turn number, omitted for Session-level Events.
- `type`: one of the catalogue below. `data`: the type's fields; consumers ignore unknown fields.

### 3. Catalogue

| `type` | Emitted | `data` (at least) |
|---|---|---|
| `session.started` | once, after configuration is valid, before the first Turn | `goal`, `size`, `max_turns`, `max_turns_cap`, `deadline_secs`, `model`, `api_format`, `stream` |
| `turn.started` | before each gateway call | `turn_budget`, `turns_left`, `final_turn`, `final_reason` (`budget`/`deadline`/null) |
| `model.delta` | per streamed fragment | `attempt`, `channel` (`text`/`thinking`/`tool_args`), `text`; `tool_args` adds `index`, `id`, `name` when known |
| `model.retry` | before a gateway retry sleeps | `attempt` (the failed one), `delay_ms`, `reason` |
| `model.replied` | after each gateway call returns | `finish_reason`, `tool_calls` (`[{id, name}]`), `output_chars`, `thinking_chars`, `usage` |
| `search.started` / `search.finished` | per `search` call | `call_id`, `queries`, `recency`; finished adds `ok`, `hits`, per-engine status (rows / no results / suspended with seconds left / challenge / timeout / throttled), `error` |
| `fetch.started` / `fetch.finished` | per `fetch` call | `call_id`, `url`, `part`; finished adds `ok`, `final_url`, `fetch_path`, `parts`, `chars`, `served_from` (`network`/`memory`/`tape`), `error` |
| `tool.failed` | per failed tool call | `call_id`, `tool`, `kind` (`dispatch`/`execution`), `reason`, `consecutive_failures`, `max_repairs` |
| `context.truncated` | when old Turns are dropped | `dropped_turns`, `max_context_chars` |
| `answer.verified` | after the grounding check | the `verification` counts and the citation count |
| `session.finished` | last Event of every Session that started emitting | `outcome`, `exit_code`, `turns_used`, `turn_budget`, `usage`, `evidence_urls`, `error` |

- `outcome` is one of `ok`, `not_configured`, `gateway_exhausted`, `gateway_rejected`, `tool_failure`, `budget_exhausted`, `session_io`, `search_blocked`, `deadline_exceeded`, `interrupted`, with the matching exit code from `docs/harness.md` (`interrupted` is `130`).
- The emitter exists once the Session id is known. An invalid `--session-id` exits 2 with no Events, since there is no Session to name; any later failure, `NotConfigured` included, ends with `session.finished`.

### 4. Two views

- Machine view: `--events <path|->` writes every Event as NDJSON (`-` is stderr). `--events-detail normal|tokens`, default `normal`, which leaves `model.delta` out. This view is for Harnesses, agents and auditors; stdout with `--json` stays one JSON object.
- Human view: Progress, `--progress auto|plain|off`. `auto` renders formatted lines on a TTY stderr and nothing otherwise, so a Harness that pipes stderr sees today's output; `plain` forces uncoloured lines; `off` disables it. `--events -` turns Progress off: one stderr, one format. `NO_COLOR` and a non-TTY drop colour; no meaning is carried by colour alone.
- `error: <message>` and `warning: …` stay on stderr as today, so a reader of `--events -` skips lines that do not start with `{`.
- `tracing` lines that duplicate an Event are removed when Progress lands; `RUST_LOG` stays for developer diagnostics.

### 5. Session v2: an incremental log

- `SESSION_FORMAT_VERSION` becomes 2. A v2 file is `header` (line 0), then `event` rows (one envelope each, `model.delta` excluded) and `turn` rows, appended as they happen; a `turn` row is written when its Turn ends, the final one carrying the Synthesis as in v1.
- `turn` and `header` rows keep their v1 fields, so the 1:1 migration to a `turns` table (ADR-0002) holds; `event` rows map to a future `events` table.
- Every exit, crash and `ctrl_c` included, leaves the rows written so far; `session.finished` is written whenever the process can still write. `ctrl_c` ends the Session with `outcome: interrupted`, exit 130.
- Defaulted path stays best-effort, explicit `--session-out` still exits 6, retention and `0700`/`0600` modes are unchanged. Readers accept v1 and v2.
- `--trace` adds `trace` rows per Turn: the messages sent (the system prompt once), the reply with thinking, tool call arguments and tool results as the model saw them. Off by default. The API key and auth headers never reach any output.

### 6. Token streaming

- `llm::Gateway` gains one observed call: the existing `chat_with_tools` path plus a caller closure, `&mut (dyn FnMut(LlmEvent) + Send)`. `chat` and `chat_with_tools` keep their signatures and call it with a no-op closure. A closure parameter is not a new `trait` declaration; ADR-0006's ban stands.
- `LlmEvent` lives in `llm` and names no `research` concept: `Text(String)`, `Thinking(String)`, `ToolArgs { index, id, name, fragment }`, `Retry { attempt, delay_ms, reason }`. `research` maps it to `model.delta` and `model.retry`. `research` still never learns the wire format (ADR-0007).
- `GATEWAY_STREAM=on|off`, default `on`; `off` keeps the single-body path. The typed `stream` field still wins over `GATEWAY_EXTRA_BODY`. A format without a streaming parser yet keeps the single-body path whatever the flag says.
- The streamed reply is assembled into the same `LlmReply` the single-body path returns for the same content: text, thinking, tool calls by index, `finish_reason`, usage (OpenAI `stream_options.include_usage`), Anthropic replay blocks with their `signature` byte for byte.
- Failures: status mapping before the first byte is unchanged. An error event or a cut stream mid-body maps to the existing `GatewayError` classes and retry rules; a retry restarts the whole attempt. The gateway calls `Retry` before sleeping, and consumers discard that attempt's fragments. Gaps longer than `GATEWAY_STREAM_IDLE_SECS` (default `30`) between chunks are a `Timeout`; the per-attempt timeout and the run deadline still bound the whole body.
- The Session log never stores fragments, only the assembled reply (in `trace` rows when `--trace` is on).

## Considered options

- A `trait Observer` with three impls: rejected. Three outputs known at build time fit concrete fields; ADR-0006 keeps `trait` for a second real adapter, and these are outputs of one adapter.
- An mpsc channel and a writer task: rejected. A crash or `ctrl_c` could drop queued Events, which defeats the audit trail, and the volume (tens of Events per Session, fragments only when asked) does not need a queue.
- Events from `web/` (search legs, Obscura): rejected. It breaks `web/`'s independence; dispatch in `research` sees every call and its result.
- Session v2 as Events only, with no `turn` rows: rejected. It breaks ADR-0002's 1:1 `turns` migration and every current reader.
- Streaming off by default until an eval proves parity: rejected by the maintainer. `on` gives live tokens by default; `off` is the escape hatch for gateways without SSE, and #155 confirms or revisits the default with eval numbers.

## Consequences

- `docs/harness.md` documents `--events`, `--events-detail`, `--progress`, `--trace`, `GATEWAY_STREAM`, `GATEWAY_STREAM_IDLE_SECS`, the envelope, the catalogue, exit 130 and Session v2. Each ticket documents what it ships.
- Exit 130 (`interrupted`) is a new Harness-visible exit code; the release is a minor bump.
- A third Event output, or Events crossing a process boundary (socket, desktop UI), revisits the "concrete fields" choice.
- Delivery order: #147 (lifecycle Events, `--events`) and #152 (OpenAI SSE, `LlmEvent`) first; then #148 (tool Events), #150 (Session v2), #153 (Anthropic SSE); then #149 (Progress), #151 (`--trace`); then #154 (fragments end to end); #155 measures and releases.
