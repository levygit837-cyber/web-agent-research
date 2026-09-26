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

| Variable | Required | Default |
|---|---|---|
| `GATEWAY_API_KEY` | yes | none; missing key exits `2` |
| `GATEWAY_BASE_URL` | no | `http://localhost:8317/v1` (any OpenAI-compatible endpoint) |
| `GATEWAY_MODEL` | no | `glm-5p2` |

`obscura` on `PATH` is optional. Without it, pages that need a browser (JS shells, anti-bot 403/429/503) fail with a typed error, and the agent moves on to other sources.

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

- `evidence_urls`: pages actually fetched in this run. Search results that were never fetched are not included.
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
