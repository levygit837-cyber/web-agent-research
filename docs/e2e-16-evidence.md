# E2E validation evidence — issue #16 (CLI research run on local gateway)

Verdict: **PASS-WITH-GAP**. Live CLI run passes on `glm-5p2`; the search and
fetch legs return canned stub observations (registry wiring gap, filed as
#25 and #26). This file is the checked-in evidence (log summary + synthesis
sample); full per-run records live in the validation plan (git-ignored).

## Passing run (Run A)

```sh
GOAL="what is the Obscura headless browser?"
GATEWAY_LIVE=1 GATEWAY_API_KEY="$GATEWAY_API_KEY" \
GATEWAY_BASE_URL="http://localhost:8317/v1" GATEWAY_MODEL="glm-5p2" \
cargo run -- research "$GOAL" --size small --max-turns 8 \
  --session-id e2e-16-ajson --session-out /tmp/e2e-16-ajson.jsonl --json
```

- Exit `0`; `turns_used=6` (FINAL at 6/8); per-turn tool calls observed
  (search every tool turn, fetch on turns 3–4).
- `--json` stdout parses as `ResearchResponse`; session JSONL has
  `7 = 1 + turns_used` lines, `SESSION_FORMAT_VERSION = 1`.
- Usage sums consistent (prompt 5909 / completion 1088 / total 6997).
- `max_turns=4` runs terminate bounded with typed `BudgetExhausted`, exit 5
  (pass-with-note, no hang, no panic).

## Synthesis sample (human stdout, Run A)

> I attempted multiple searches (general, scraping-focused,
> stealth/anti-detection, GitHub, and Puppeteer-alternative angles) and
> fetched the one URL the searches returned. Unfortunately, every result
> pointed to the same placeholder page (`https://example.com/obscura`),
> which is a reserved example domain containing only a stub message. No
> substantive, verifiable content about an "Obscura headless browser" was
> returned by any search. … Rather than invent details, I'm reporting that
> the available tool results were insufficient to answer the question
> accurately.
>
> Sources: 1. [Obscura engine overview](https://example.com/obscura)

No invented URLs: the sole citation traces to a tool observation, and the
model explicitly refused to treat the stub as product evidence.

## Other runs

- Run 0 (hermetic CLI smoke): PASS, 6/6 handler tests.
- Run B (per-leg): `gateway_live` 2/2 live PASS; `search_live` 2/2 PASS
  (DDG 20 rows live; Startpage zero-row/challenge path, suite-accepted);
  `fetch_obscura` hermetic 16/16 PASS; `live_obscura` skipped
  (binary absent — gated, not failed).
- Run C (`glm-5.3-flash` probe, evidence only): exit 5 `BudgetExhausted`
  after 2 turns; the expected 429/CREDIT wall did NOT occur. Not a verdict
  input; no re-probe.
- Run D (stub-vs-live audit): `ToolRegistry::live()` returns canned stub
  results for both legs; real engines (`web_engine_search::fanout`,
  `Obscura::fetch_markdown`) exist and are proven but unwired.

## Dependency checklist

| Dep | Outcome |
|-----|---------|
| #11 gateway chat | CONFIRMED LIVE |
| #13 search fan-out | ENGINE LIVE / REGISTRY STUB — gap, see #25 |
| #14 fetch markdown | HERMETIC RAN / LIVE GATED (binary absent) + REGISTRY STUB — gap, see #26 |
| #12 loop turns | CONFIRMED LIVE |
| #15 CLI print | CONFIRMED LIVE |
| #20 extraction | CONFIRMED (no code changes) |

## Follow-ups filed

- #25 — search leg: `ToolRegistry::live()` returns canned stub, real fan-out unwired.
- #26 — fetch leg: `ToolRegistry::live()` returns canned stub, Obscura unwired.

Checks: `cargo test --locked`, `cargo fmt --check`, `cargo clippy
--all-targets -- -D warnings` — all green. No code changes by this
validation. Secrets never written to files (key read at runtime from
`~/.factory/settings.json` customModels only).
