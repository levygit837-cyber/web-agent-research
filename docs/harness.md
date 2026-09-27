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

## Latency

Measured tool numbers, not end-to-end run time:

- Search fan-out: ~1.7 s live for 1 query returning 5 Hits.
- Static fetch (reqwest + htmd, no browser): 0.25–0.96 s — docs.rs 0.25 s, react.dev 0.41 s, wikipedia 0.63 s, github.com 0.96 s.
- Browser fallback (Obscura, JS shells/challenge pages): 1.5–8.2 s.

End-to-end run time is dominated by LLM turns, bounded by `--max-turns`, not by search/fetch. A full live end-to-end timing run is pending #33.

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

Errors are printed to stderr as `error: <message>`; stdout stays empty.

## Harness snippet

Add this to the Harness's agent instructions (omp `AGENTS.md`, Claude Code `CLAUDE.md`, or similar):

```md
For questions that need current web information, run
`web-agent-research research "<question>" --json --size small` with the shell tool
and answer from `synthesis`; cite `synthesis.citations[].url`. Exit code != 0 means
no answer; stderr says why.
```
