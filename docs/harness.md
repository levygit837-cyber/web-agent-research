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
| `GATEWAY_BASE_URL` | no | `http://localhost:8317/v1` (any OpenAI-compatible endpoint) | n/a (any non-empty string accepted) |
| `GATEWAY_MODEL` | no | `glm-5p2` | n/a (any non-empty string accepted) |
| `GATEWAY_REASONING_EFFORT` | no | unset (no reasoning-effort sent) | n/a (any non-empty string accepted) |
| `GATEWAY_MAX_TOKENS` | no | unset (no `max_completion_tokens` cap sent) | not a valid `u32` exits `2` (`NotConfigured`) |
| `GATEWAY_TIMEOUT_SECS` | no | `60` (per-attempt request timeout) | not a valid integer exits `2` (`NotConfigured`) |
| `GATEWAY_MAX_ATTEMPTS` | no | `3` (total attempts incl. the first try) | not a valid `u32` exits `2` (`NotConfigured`) |

`obscura` on `PATH` is optional. Without it, pages that need a browser fallback (JS shells, challenge pages, non-text content) fail with a typed error, and the agent moves on to other Hits.

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
  "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0, "reasoning_tokens": 0 }
}
```

- `evidence_urls`: pages actually fetched in this run. Hits that were never fetched are not included.
- Without `--json`, stdout is human-readable markdown: summary, `##` themes, and a numbered `Sources:` list.

## Exit codes

| Code | Meaning | Harness action |
|---|---|---|
| 0 | Synthesis on stdout | use it |
| 2 | Not configured (empty goal, missing key, `max_turns < 1`) | fix env/args; do not retry |
| 3 | Gateway exhausted: retries/rate limit, or auth failed (`bad API key` in stderr) | auth: fix key; otherwise retry later |
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
