# Harness contract: calling Web Search

A Harness calls Web Search as one shell command and reads one JSON object from stdout. The calling model sees only the Synthesis, never raw pages.

## Command

```sh
web-agent-research research "<goal>" --json [--size small|medium|large] [--max-turns N]
```

- `--size` (default `medium`): answer length.
- `--max-turns` (default `8`): total LLM calls allowed; a run that reaches this without an answer exits `5`.
- `--session-id`, `--session-out`: optional. By default the Session is written to `sessions/<unix-secs>-<pid>.jsonl`.

## Environment

| Variable | Required | Default | Invalid value |
|---|---|---|---|
| `GATEWAY_API_KEY` | yes | none | empty/missing exits `2` (`NotConfigured`) |
| `GATEWAY_API_FORMAT` | no | `openai` | `openai` (Chat Completions, `POST {base}/chat/completions`, Bearer auth) or `anthropic` (Messages, `POST {base}/messages`, `x-api-key` + `anthropic-version: 2023-06-01`), case-insensitive; anything else exits `2` (`NotConfigured`) |
| `GATEWAY_BASE_URL` | no | `http://localhost:8317/v1` (the endpoint path is appended per `GATEWAY_API_FORMAT`) | n/a (any non-empty string accepted) |
| `GATEWAY_MODEL` | no | `muse-spark-1.3` | n/a (any non-empty string accepted) |
| `GATEWAY_REASONING_EFFORT` | no | unset (nothing sent) | must be one of `none\|minimal\|low\|medium\|high\|xhigh\|max`; anything else exits `2` (`NotConfigured`). OpenAI format: top-level `reasoning_effort`. Anthropic format: `output_config.effort`, and `none`/`minimal` exit `2` (no Messages equivalent) |
| `GATEWAY_MAX_TOKENS` | no | OpenAI format: unset (no `max_completion_tokens` cap sent). Anthropic format: `8192` plus any enabled thinking budget (`max_tokens` is required there) | not a valid `u32` exits `2` (`NotConfigured`) |
| `GATEWAY_THINKING_BUDGET` | no | unset (no `thinking` sent) | not a valid `u32` exits `2`; `0` sends `thinking: {"type": "disabled"}`; `> 0` sends `{"type": "enabled", "budget_tokens": n}`; if `GATEWAY_MAX_TOKENS` is also set and is `<=` this budget, exits `2` (reasoning tokens count against `max_tokens`). Anthropic format: `1..=1023` exits `2` (API minimum is 1024) |
| `GATEWAY_PROMPT_CACHE_KEY` | no | enabled: every OpenAI-format request carries `prompt_cache_key` set to the run's Session id | `off` disables it (field omitted); any other value is ignored and caching stays enabled. Not sent in the Anthropic format, which caches through `cache_control` breakpoints instead |
| `GATEWAY_EXTRA_BODY` | no | unset (no extra fields merged) | must be a JSON object; invalid JSON or a non-object value exits `2` (`NotConfigured`) |
| `GATEWAY_TIMEOUT_SECS` | no | `60` (per-attempt request timeout) | not a valid integer exits `2` (`NotConfigured`) |
| `GATEWAY_MAX_ATTEMPTS` | no | `3` (total attempts incl. the first try) | not a valid `u32` exits `2` (`NotConfigured`) |

The binary reads only the process environment; it does not load `.env` itself. Locally, keep these in the git-ignored `.env` and export them before a run: `set -a; . ./.env; set +a`.

`GATEWAY_EXTRA_BODY` merges into the wire request body (OpenAI SDK `extra_body` semantics) in both formats: useful for gateway-specific fields this client has no typed support for yet, e.g. `{"reasoning": {"effort": "high"}, "prompt_cache_retention": "24h", "verbosity": "low"}` (OpenAI) or `{"service_tier": "auto"}` (Anthropic). On a key collision, the typed fields this client sends (`model`, `messages`, `system`, `max_tokens`/`max_completion_tokens`, `reasoning_effort`/`output_config`, `thinking`, `prompt_cache_key`, `tools`, `tool_choice`) always win; `GATEWAY_EXTRA_BODY` only fills in keys this client does not otherwise send.

### Anthropic format

- Transcript mapping: system messages become the top-level `system` array; each tool turn becomes one `assistant` message (any `thinking`/`redacted_thinking` blocks first, echoed back verbatim with their `signature`, then text, then `tool_use` blocks) followed by one `user` message holding only the `tool_result` blocks, so every `tool_use` id is answered in the next message.
- Prompt caching: up to three `cache_control: {"type": "ephemeral"}` breakpoints per request (last tool when tools are offered, last system block, last block of the newest message). The prefix order is `tools → system → messages` and the transcript is append-only, so each turn reads what the previous one wrote. Below the model's minimum cacheable prefix (e.g. 4,096 tokens on Claude Haiku 4.5, 1,024 on Sonnet 4.5) nothing is cached and no error is returned.
- Usage: `prompt_tokens` is the full input (`input_tokens + cache_creation_input_tokens + cache_read_input_tokens`), `cached_prompt_tokens` the cache reads, `cache_creation_prompt_tokens` the cache writes.
- `stop_reason` maps to the OpenAI vocabulary internally (`end_turn`/`stop_sequence` → `stop`, `max_tokens`/`model_context_window_exceeded` → `length`, `tool_use` → `tool_calls`); `refusal` fails the turn.
- With thinking enabled the API only accepts `tool_choice` `auto`/`none`; this client always sends `auto`.

### Provider notes

- Anthropic's OpenAI-compatible layer does not support prompt caching (`prompt_cache_key` has no effect there); use `GATEWAY_API_FORMAT=anthropic` against a native Messages endpoint for `cache_control` breakpoints.
- Reasoning/thinking tokens count against `max_completion_tokens`/`max_tokens`, not a separate budget: `GATEWAY_THINKING_BUDGET` must stay strictly below `GATEWAY_MAX_TOKENS` when both are set (some providers, e.g. Anthropic via OpenRouter, enforce this themselves and error otherwise).
- Claude Haiku 4.5 supports manual thinking only (`budget_tokens`), not `output_config.effort`: leave `GATEWAY_REASONING_EFFORT` unset for it.

### Search cache

`web::search::cache` (#67) persists search-leg results per (engine, normalized query, recency, page) under `<cache_root>/search/` so a repeated `search` call across turns or runs skips the HTTP request entirely. Cache root resolution (`web::cache_dir`): `SEARCH_CACHE_DIR`, else `$XDG_CACHE_HOME/web-agent-research`, else `$HOME/.cache/web-agent-research`; when none of those resolve (both `HOME` and `XDG_CACHE_HOME` unset/empty) caching is silently disabled, never an error.

| Variable | Required | Default | Invalid value |
|---|---|---|---|
| `SEARCH_CACHE_DIR` | no | unset (falls back to `XDG_CACHE_HOME`/`HOME`) | n/a (any non-empty path accepted) |
| `SEARCH_CACHE` | no | enabled | `off` disables both lookup and store; any other value is ignored and caching stays enabled |
| `SEARCH_CACHE_TTL_SECS` | no | `86400` (24h), `3600` (1h) when the `search` call's `recency` is `day` | not a valid `u64` is ignored, default TTL applies |
| `SEARCH_CACHE_MAX_BYTES` | no | `20971520` (20 MiB) of `search/`, oldest-first eviction by fetched-at | not a valid `u64` is ignored, default cap applies |

To clear the cache, delete the resolved cache root's `search/` directory (`rm -rf "${SEARCH_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/web-agent-research}/search"`), or set `SEARCH_CACHE_DIR` to an empty/fresh directory for one run.

### Engine governor

`web::search::governor::Governor` (#62, #63, #64, #66) skips walled engines, paces requests like a person, and keeps Startpage off by default. State (suspension + last-request time) persists to `<cache_root>/engines.json` (same root as the search cache above); a corrupt or missing file loads as empty, never an error.

| Variable | Required | Default | Invalid value |
|---|---|---|---|
| `SEARCH_ENGINES` | no | `duckduckgo,brave,yahoo,bing` (Startpage stays opt-in only, #64: it serves an Anubis proof-of-work challenge to bot-detected clients, and soliciting it would deliberately bypass that wall) | comma list, case-insensitive `duckduckgo`/`ddg`, `startpage`/`sp`, `brave`, `yahoo`, `bing`; unknown tokens are ignored; a result that ends up empty after filtering falls back to the default |
| `SEARCH_BING_MARKET` | no | `en-US` (`mkt` sent on every Bing request; `setlang` is always derived from its language subtag, e.g. `pt-BR` → `setlang=pt`. #66: Bing silently geo/language-localizes on client IP with zero explicit signal otherwise — measured 0/10 relevant results from a non-US IP with no `mkt`, 5/5 after adding it) | an empty/whitespace-only override falls back to the default; any other non-empty string is sent verbatim as `mkt` |
| `SEARCH_SUSPEND_CHALLENGE_SECS` | no | `3600` | not a valid `u64` falls back to the default |
| `SEARCH_SUSPEND_403_SECS` | no | `180` | not a valid `u64` falls back to the default |
| `SEARCH_SUSPEND_429_SECS` | no | `180` (a response's `Retry-After` header, when present, wins over this default) | not a valid `u64` falls back to the default |
| `SEARCH_SUSPEND_UPSTREAM_SECS` | no | `30` (other non-2xx/non-403/429 responses; `0` disables suspension for this kind) | not a valid `u64` falls back to the default |
| `SEARCH_SUSPEND_TIMEOUT_SECS` | no | `0` (a fan-out `Timeout` never suspends by default; a slow leg is as likely to be the local network or the fan-out deadline as a bot wall) | not a valid `u64` falls back to the default |
| `SEARCH_PACE_MIN_MS` / `SEARCH_PACE_MAX_MS` | no | `1500` / `4000` (global default gap between requests to one engine) | not a valid `u64` falls back to the default; a max at or below min degenerates to that fixed gap |
| `SEARCH_PACE_<ENGINE>_MIN_MS` / `_MAX_MS` (`<ENGINE>` = `DUCKDUCKGO`/`STARTPAGE`/`BRAVE`/`YAHOO`/`BING`) | no | falls back to `SEARCH_PACE_MIN_MS`/`_MAX_MS` | same as above |
| `SEARCH_MAX_REQUESTS_PER_ENGINE` | no | `200` (per-run request cap; further requests degrade to `Throttled`, not a bot-wall signal) | not a valid `u64`/`u32` falls back to the default |
| `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL` | no | `4` (cap on Queries dispatched to one engine within a single call; see the pacing note below) | not a valid `u64` falls back to the default |

Engine priority for consensus merge and leg dispatch order (`SearchProvider::priority`, from the #65 evaluation's measured relevance/breadth data): Startpage, Brave, Yahoo, DuckDuckGo, Bing (lowest value first). Page 1 only per engine for #66; pagination and per-engine budget tuning are #73's job.

Suspension: a `Challenge`, HTTP `403`/`429`, other `Upstream` (5xx/transport), or `Retry-After` leg error suspends that engine for the mapped duration (idea inspired by SearXNG's `search.suspended_times`, AGPL-3.0, no code reuse); a suspended engine is skipped with zero HTTP requests for the rest of the suspension. A later, shorter suspension never overwrites a longer one still active. Only a suspension recorded from a live `Challenge` counts as a bot-wall signal for the check below -- a suspension from a 5xx/transport/429/403/`Retry-After` failure does not. If every enabled engine is Challenge-suspended (or live-Challenged), the fan-out fails with a typed error naming the engines and the time left, which reaches the same exit `7` `SearchBlocked` path below (each engine's own challenge signal: DuckDuckGo's `anomaly-modal` body marker, Startpage's Anubis proof-of-work/CAPTCHA, Brave's HTTP 429 with no body marker, Yahoo's redirect to a `guce.yahoo.com`/`consent.yahoo.com` wall, Bing's `challenge/verify` JS marker or HTTP 429).

Pacing: requests to one engine are serialized (via a per-engine queue held for the whole leg); different engines run in parallel with each other. Right after acquiring the queue, the gate rechecks suspension and the per-run budget -- closing the race where a sibling leg for the same engine settled while this leg was still queued -- before waiting the jittered gap and dispatching. `SOFT_DEADLINE_SECS`/`HARD_DEADLINE_SECS` (5 s / 30 s, unchanged) still bound one fan-out call: the default `SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL` of 4 is sized so the 4th Query to one engine waits at most `3 * SEARCH_PACE_MAX_MS` (12 s at defaults) before its request even starts, leaving comfortable headroom inside the 30 s hard deadline for the round-trips themselves (measured live fan-out latency is ~1.7 s per query, see "Latency" below).

Hermetic rule: `Searcher::with_bases` and every test use a `Governor` that never persists, never paces, has no cap, and always enables every engine, regardless of ambient `SEARCH_*` env vars -- the suite never touches the real cache or the network's pacing state.

## Latency

Measured tool numbers, not end-to-end run time:

- Search fan-out: ~1.7 s live for 1 query returning 5 Hits (unpaced; the engine governor's per-engine pacing gap, default 1500..4000 ms, applies on top of this for a second Query to the same engine, and persists across back-to-back CLI runs).
- Static fetch (reqwest + htmd, no browser): 0.25–0.96 s — docs.rs 0.25 s, react.dev 0.41 s, wikipedia 0.63 s, github.com 0.96 s.
- Browser fallback (Obscura, JS shells/challenge pages): 1.5–8.2 s.

End-to-end run time is dominated by LLM turns, bounded by `--max-turns`, not by search/fetch. Measured live (#33, `docs/e2e-evidence.md`): 7.7 s for a 2-turn run on `claude-haiku-4.5` that fetched 2 pages.

## Browser profile bump (#59)

`src/web/profile.rs` pins Chrome stable and its two previous majors (`STABLE_MAJORS`), across macOS/Windows/Linux, so `web/search`'s DDG and Startpage legs, and `web/fetch`'s static fetch path, never send a UA family/version that DDG, Startpage, or a fetched host can flag as stale. One profile is drawn at random per request chain (one engine leg plus its follow-ups for DDG/Startpage, one per fetch call for the static fetch path: DDG's `s`/`vqd` pagination re-POSTs and the Startpage homepage → search POST both reuse the same profile, because DDG's `vqd` token is bound to the UA).

To bump when Chrome ships a new stable major:

1. Check `https://versionhistory.googleapis.com/v1/chrome/platforms/{mac,win,linux}/channels/stable/versions?pageSize=3` for the current stable major per platform.
2. Update `STABLE_MAJORS` in `src/web/profile.rs` to the new stable major and its 2 predecessors.
3. Run `cargo test -- --ignored profile_table_matches_chrome_stable` (a live network call) to confirm the table now matches. This is a manual step: no CI job runs `--ignored`/`--include-ignored`, so a lagging table is caught only by running this command, not automatically.
4. `cargo test web::profile::` for the coherence suite (`every_profile_is_internally_coherent`): every profile's UA major must equal its `sec_ch_ua` version, and its platform must equal `sec_ch_ua_platform`.

Only Chrome-family profiles exist while the transport is plain `reqwest`/rustls (no TLS/JA3 emulation, #56/#60): a Firefox or Safari UA over a generic rustls ClientHello would be its own family/TLS mismatch signal.

## Output (`--json`, exit 0)

```json
{
  "session_id": "1790000000-4242",
  "synthesis": {
    "size": "medium",
    "summary": "…",
    "themes": [{ "title": "…", "points": ["…"], "citations": [{ "url": "https://…", "title": "…" }] }],
    "citations": [{ "url": "https://…", "title": "…" }]
  },
  "turns_used": 4,
  "evidence_urls": ["https://…"],
  "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0, "reasoning_tokens": 0, "cached_prompt_tokens": 0, "cache_creation_prompt_tokens": 0 }
}
```

- `evidence_urls`: pages actually fetched in this run. Hits that were never fetched are not included.
- `citations` (top-level and per theme): only links to pages fetched in this run, matched against each page's final (post-redirect) URL after normalization (`www.` and trailing-slash variants count as the same page). Links the model copied from inside a fetched page, or to Hits it never fetched, are dropped from `citations`; the prose in `summary`/`points` is left as written.
- Without `--json`, stdout is human-readable markdown: summary, `##` themes, and a numbered `Sources:` list.

## Exit codes

| Code | Meaning | Harness action |
|---|---|---|
| 0 | Synthesis on stdout | use it |
| 2 | Not configured (empty goal, missing key, `max_turns < 1`) | fix env/args; do not retry |
| 3 | Gateway exhausted: retries/rate limit, auth failed (`bad API key`, HTTP 401), or access refused (`refused access (HTTP 403): <upstream message>`) | 401: fix key; 403: the key works but the model/tier is gated upstream, pick another model; otherwise retry later |
| 4 | Tool failures exceeded the repair budget | retry, or rephrase the goal |
| 5 | Turn budget exhausted | retry with a higher `--max-turns` |
| 6 | Session file I/O | check `--session-out` path |
| 7 | Search blocked: the run finalized with no fetched Evidence, no `search` call ever returned a Hit, and every leg of at least one `search` call was either a bot-detection Challenge (DuckDuckGo anomaly page or Startpage Anubis proof-of-work/CAPTCHA) or a skip of an engine already suspended *from* such a Challenge (#62) — the search engines are walled from this network, not that nothing exists | retry later or from a different network/IP; do not treat as "no results" |

Errors are printed to stderr as `error: <message>`; stdout stays empty.

## Harness snippet

Add this to the Harness's agent instructions (omp `AGENTS.md`, Claude Code `CLAUDE.md`, or similar):

```md
For questions that need current web information, run
`web-agent-research research "<question>" --json --size small` with the shell tool
and answer from `synthesis`; cite `synthesis.citations[].url`. Exit code != 0 means
no answer; stderr says why.
```
