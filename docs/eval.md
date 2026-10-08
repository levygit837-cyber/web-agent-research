# Offline eval

Measures whether a change improves answers. Eval-only: nothing here is part of the Harness contract (`docs/harness.md`), and `WAR_EVAL_RECORD`/`WAR_EVAL_REPLAY` may change or go away without notice.

Run-to-run noise is larger than most effects (`docs/harness.md`: citation counts 3, 15, 20 across repeats of one goal), so compare paired runs over the whole golden set with K repeats, never single runs.

## Pieces

- Golden set: `tests/golden/<category>.json`, 7 categories × 8 goals. Each goal has 3-6 atomic nuggets `{text, url, volatility: never|slow|fast, verified_on}`, each checked against `url` on `verified_on`. Re-verify `fast` nuggets before trusting a recall drop on them.
- Tool tape (`src/research/agent_loop/tape.rs`): records or replays the `search` and `fetch` tools on the `ToolRegistry` path.
- Driver: `cargo run --example eval -- <run|judge|summary|compare|pack>`, code in `tests/eval/`.
- Offline gate: `tests/eval_offline.rs`, part of `cargo test`, no network.

## Models

- Agent: every eval run uses `claude-haiku-4.5` (`GATEWAY_MODEL`), the model the agent runs in production. `claude-haiku-5.5` replaces it as the main model once the gateway offers it; until then, results from any other agent model are not comparable to the baseline.
- Judge: a larger model than the agent, never the agent's own model: `gpt-5.6-sol` by default (`--model` takes another larger model). The judge grades; it is not the system under test.

## Tool tape

| Env var | Effect |
|---|---|
| `WAR_EVAL_RECORD=<dir>` | Tools run live. Each `search` outcome is appended to `<dir>/search.jsonl`, each `fetch` outcome (cleaned `Evidence.markdown`, or the failure) to `<dir>/pages.jsonl`, keyed by the normalized URL. |
| `WAR_EVAL_REPLAY=<dir>` | No network for tools. `fetch` serves the recorded page with no TTL. |

Setting both exits `2`.

Replay rules:

- `fetch`: the recorded page of the requested URL (normalized like search dedup), or of a recorded redirect target. A URL with no recording fails the call with `eval replay miss: …`, also printed to stderr and counted as `replay_misses` in the metrics.
- `search`: the model's queries rarely repeat verbatim across runs, so replay does not depend on exact query strings. A call whose normalized query set (lower-cased, whitespace-collapsed, sorted, deduped) was recorded gets that outcome verbatim. Otherwise it gets the recorded Hits that any of its normalized queries surfaced, else the union of every recorded Hit for the goal; deduped by URL, Hits with a recorded page first, cut to `top_k`.

Fixture files are JSON Lines and append across runs, so K recordings of one goal share one fixture dir.

A `replay` run with a live gateway is a new run: the model may fetch a URL the recording never fetched. That call fails with a replay miss, and the model sees an ordinary failed fetch. On the 2026-10-05 baseline, two replays had a miss in 20 and 23 of 56 runs (30 and 37 misses). Check `replay_misses` before reading a difference as an effect, and record K>1 to widen the fixtures.

## Record, replay, live

Build the CLI first; the driver runs `target/release/web-agent-research research <goal> --json` once per goal and repeat.

```sh
cargo build --release
set -a; . ./.env; set +a
cargo run --example eval -- run --mode record --fixtures target/eval/fixtures --repeats 1 --out base.json
```

| Mode | Tools | Gateway | Use |
|---|---|---|---|
| `record` | live, taped into `<fixtures>/<goal id>/` | live, through a recording proxy into `gateway.r<k>.jsonl` | Baseline; new fixtures. |
| `replay` | from `<fixtures>/<goal id>/` | live | A change to the prompt, loop or model, on fixed web content. |
| `live` | live, untaped | live | End-to-end, including search and fetch changes. |

Flags: `--repeats` (default 3), `--goal <id>` (repeatable; default all), `--pause-secs` between runs that touch the live web (default 20; engines bot-wall a fast loop, exit `7`), `--bin`, and extra `research` flags after `--`. The results file is rewritten after every run; rerunning with the same `--out` resumes.

`record` also writes `run.r<k>.json` (goal, flags, `GATEWAY_API_FORMAT`, `GATEWAY_MODEL`, metrics) next to the transcript.

## Metrics

One row per run, from the exit code and the `--json` stdout: `exit_code`, `turns_used`, `turn_budget`, token counts, `citations`, `evidence_urls`, `verification` (bullet grounding counts, when present), `replay_misses`, `wall_ms`, and the rendered answer. `eval judge` adds `judge` (nuggets, SimpleQA class) and `claims` (one verdict per bullet).

## Judge

```sh
cargo run --example eval -- judge base.json [--model gpt-5.6-sol] [--fixtures target/eval/fixtures]
```

One gateway call per answered run, with the same gateway env and a pinned model (default `gpt-5.6-sol`, a different family from the agent's `claude-*`), recorded as `judge_model` in the results file. It returns:

- per nugget: supported or not (same fact, same specific values);
- a SimpleQA class: `CORRECT`, `INCORRECT` (contradicts a nugget, or the core claim is wrong) or `NOT_ATTEMPTED`.

Runs that exited non-zero are `NOT_ATTEMPTED` without a call. Judging is resumable and refuses a file already judged by another model.

### Claim pass

Recall asks whether the answer states the golden facts. The claim pass asks the converse: is every bullet true to the pages it cites? The Harness trusts each cited bullet, and the grounding check (`verification`) compares quotes and tokens, not claims, so it cannot stand in for this.

After the nugget grade, each answered run gets one more call to the same pinned judge model (a second only when the reply does not parse):

1. The rendered answer is split into bullets: the `-` lines under the `##` themes, before `Sources:`. The summary paragraph is not a bullet. The `(quote: "…")` suffix is cut off the bullet text, and the links in the bullet are its citations.
2. Each link is matched, by normalized URL (`dedup_key`, on the requested or the final URL), to a page the run fetched, read from `<fixtures>/<goal id>/pages.jsonl` (`--fixtures`, the dir the run was recorded into; `replay` runs use the same dir). The pages are sent whole; a page over 150k characters, or a call over 500k, is cut and marked.
3. A bullet with no link, or only links to pages the run never fetched, is `uncited` with no judgement.
4. The judge sees the question, the cited pages and the remaining bullets, and judges only from the page text, with one verdict per bullet:
   - `supported`: the cited pages state the main point and every specific detail (names, numbers, versions, flags, signatures, code);
   - `partly`: the main point is on the pages, a specific detail is not;
   - `unsupported`: the main point is not on the cited pages;
   - `contradicted`: a cited page says the opposite.

Runs that exited non-zero have no answer and no claims. A `live` results file has no recorded pages, so `eval judge` skips the claim pass there and its claim metrics stay missing. A run already judged before the claim pass existed gets only the claim pass when judged again.

Metrics, per run, as shares of all the run's bullets (so `uncited` counts against precision):

| metric | meaning |
|---|---|
| `claim_precision` | `supported` / bullets |
| `claim_partly` | `partly` / bullets |
| `claim_unsupported` | `unsupported` / bullets |
| `claim_contradicted` | `contradicted` / bullets |
| `uncited_share` | `uncited` / bullets |

The five shares add up to 1. `claim_precision` over checked bullets only is `supported / (1 - uncited_share)`.

## Summary and compare

```sh
cargo run --example eval -- summary base.json
cargo run --example eval -- compare a.json b.json
```

`summary` prints one Markdown row per category: exit-0 share, SimpleQA shares, mean nugget recall, pooled exact-quote share, and means of citations, turns, tokens and wall time. When any run has claims, a second table follows with the claim shares per category, pooled over bullets.

`compare` pairs by goal: per metric, each goal in both files gives `d = mean(B over repeats) - mean(A over repeats)`, and the report is the mean of `d` with a two-sided 95% Student-t interval. A difference whose interval contains 0 is noise at this sample size.

## Fixtures and the offline gate

`tests/eval_fixtures/<goal id>/` holds gzipped recordings (`*.json.gz`, `*.jsonl.gz`), at least one answered goal per category. Pages are kept whole: the grounding check reads them, so trimming one changes the metrics.

`tests/eval_offline.rs` checks the golden set's shape and replays every committed recording: the CLI runs with `WAR_EVAL_REPLAY` and a clean env against a stub gateway that answers with the recorded transcript, and the replayed metrics must equal the recorded ones with no replay miss and the same number of gateway calls.

To add or refresh a fixture:

```sh
cargo run --example eval -- run --mode record --fixtures /tmp/fix --repeats 1 --goal how_to-03 --out /tmp/fix.json
cargo run --example eval -- pack /tmp/fix how_to-03
cargo test --test eval_offline
```

A change to the agent loop, prompts or tool rendering can make a committed transcript diverge (different gateway call count or metrics); re-record the affected fixtures in the same PR.

## Baseline

- `docs/eval/baseline-2026-10-05.json`: the whole golden set, K=1, `record` mode at `80b1040`, `claude-haiku-4.5`, judged by `gpt-5.6-sol`.
- `docs/eval/baseline-2026-10-07.json`: the same at `46956ca` (after #116-#120): 79% correct, nugget recall 0.55. Against 2026-10-05, every paired 95% CI on correct and recall contains 0. Compare new changes against this file.
