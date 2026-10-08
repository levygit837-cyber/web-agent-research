# Offline eval

Measures whether a change improves answers. Eval-only: nothing here is part of the Harness contract (`docs/harness.md`), and `WAR_EVAL_RECORD`/`WAR_EVAL_REPLAY`/`WAR_EVAL_REPLAY_THROUGH` may change or go away without notice.

Run-to-run noise is larger than most effects (`docs/harness.md`: citation counts 3, 15, 20 across repeats of one goal; `correct` moved -0.089 [-0.217, +0.038] between two K=1 runs of nearly the same code). A decision needs K>=3 paired runs over the whole golden set; a K=1 difference is a hint, never a result. `eval run` defaults to `--repeats 3` for that reason.

## Pieces

- Golden set (v2): `tests/golden/<category>.json`, 7 categories × 8 goals. Each goal has 3-12 nuggets `{text, scope: core|supporting, url, volatility: never|slow|fast, verified_on}`, each checked against `url` on `verified_on`. Re-verify `fast` nuggets before trusting a recall drop on them. See [Golden set rules](#golden-set-rules).
- Tool tape (`src/research/agent_loop/tape.rs`): records or replays the `search` and `fetch` tools on the `ToolRegistry` path.
- Driver: `cargo run --example eval -- <run|judge|summary|compare|pack>`, code in `tests/eval/`.
- Offline gate: `tests/eval_offline.rs`, part of `cargo test`, no network.

## Golden set rules

- Atomic: a nugget is one independently checkable claim, one sentence, at most 200 characters. An answer that states half of a compound claim must still score for that half, so a nugget never joins claims with `;`, a second sentence, or `, but` / `, while` / `, whereas` / `, so` / `, and then` / ` and also` (text inside `code` or "quotes" is exempt). `golden::load` rejects a file that breaks this.
- Scope, written in the file, never inferred per run (two classification passes of the same run disagreed on 30 of 305 nuggets):
  - `core`: the goal's question asks for this fact. Every goal has at least 2.
  - `supporting`: true, related detail a good answer may add and the question does not ask for.
  - A fact outside the goal has no scope: it is not in the set.
- General, not example-bound: a nugget states the fact, not the placeholder its source happens to use (a sample crate name, a sample `count: u8`), so a correct answer with another placeholder still matches. A flag name, an error code, a version or a documented default is the fact and stays.
- Verified: every new or rewritten nugget is re-read on its `url` and dated `verified_on` that day.

## Models

- Agent: every eval run uses `claude-haiku-4.5` (`GATEWAY_MODEL`), the model the agent runs in production. `claude-haiku-5.5` replaces it as the main model once the gateway offers it; until then, results from any other agent model are not comparable to the baseline.
- Judge: a larger model than the agent, never the agent's own model: `gpt-5.6-sol` by default (`--model` takes another larger model). The judge grades; it is not the system under test.

## Tool tape

| Env var | Effect |
|---|---|
| `WAR_EVAL_RECORD=<dir>` | Tools run live. Each `search` outcome is appended to `<dir>/search.jsonl`, each `fetch` outcome (cleaned `Evidence.markdown`, or the failure) to `<dir>/pages.jsonl`, keyed by the normalized URL. |
| `WAR_EVAL_REPLAY=<dir>` | No network for tools. `fetch` serves the recorded page with no TTL. |
| `WAR_EVAL_REPLAY_THROUGH=<dir>` | `search` replays as above. `fetch` serves the recorded page; a URL with no recording is fetched live, through the normal fetcher and egress policy, and appended to `<dir>/pages.jsonl` in the same record format (a failure is appended too). |

Setting more than one exits `2`.

Replay rules:

- `fetch`: the recorded page of the requested URL (normalized like search dedup), or of a recorded redirect target. A URL with no recording fails the call with `eval replay miss: …`, also printed to stderr and counted as `replay_misses` in the metrics.
- `search`: the model's queries rarely repeat verbatim across runs, so replay does not depend on exact query strings. A call whose normalized query set (lower-cased, whitespace-collapsed, sorted, deduped) was recorded gets that outcome verbatim. Otherwise it gets the recorded Hits that any of its normalized queries surfaced, else the union of every recorded Hit for the goal; deduped by URL, Hits with a recorded page first, cut to `top_k`.

Replay-through rules:

- Searches stay taped: a live search would bot-wall the IP and change the Hits. Only `fetch` misses go live.
- Each live fetch prints one `eval replay miss: fetching <url> live, …` line to stderr, so `replay_misses` now counts the live fetches a run made, and a reader sees how much of it was taped. A page fetched live once is a recording from then on: the next run replays it and counts no miss.
- The dir must already hold a fixture (`search.jsonl`, `pages.jsonl`); a mistyped dir exits `2` instead of starting an empty tape.

Fixture files are JSON Lines and append across runs, so K recordings of one goal share one fixture dir.

A `replay` run with a live gateway is a new run: the model may fetch a URL the recording never fetched. That call fails with a replay miss, and the model sees an ordinary failed fetch. On the 2026-10-05 baseline, two replays had a miss in 20 and 23 of 56 runs (30 and 37 misses); on 2026-10-07, the `--size large` replay had 41 misses in 24 of 56 runs. That biases an A/B against the arm that reads more (coverage, gap searches): the tape penalizes it, not the change. Check `replay_misses` before reading a difference as an effect, and record K>1 to widen the fixtures.

Use `replay-through` when the change under test makes the agent read more pages than the recording did. It removes the failed-fetch penalty and leaves the Hits unchanged. Keep plain `replay` when the change should see exactly the recorded web, e.g. the offline gate.

`replay-through` appends to `pages.jsonl`, so it changes the fixtures it runs on. Each arm of an A/B must start from its own copy of the same frozen tape dir (`cp -R` the dir per arm), never share one: the second arm would find the first arm's pages already taped and read a different web. Never point it at `tests/eval_fixtures` or a baseline you still need frozen.

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
| `replay-through` | from `<fixtures>/<goal id>/`; fetch misses live, appended to its `pages.jsonl` | live | A change that reads more pages than the recording did (coverage, gap searches). Copy the frozen dir per arm. |
| `live` | live, untaped | live | End-to-end, including search and fetch changes. |

Flags: `--repeats` (default 3), `--goal <id>` (repeatable; default all), `--pause-secs` between runs that touch the live web (default 20; engines bot-wall a fast loop, exit `7`; also applied in `replay-through`, which touches the web for fetch misses), `--bin`, and extra `research` flags after `--`. The results file is rewritten after every run; rerunning with the same `--out` resumes.

`record` also writes `run.r<k>.json` (goal, flags, `GATEWAY_API_FORMAT`, `GATEWAY_MODEL`, metrics) next to the transcript.

## Metrics

One row per run, from the exit code and the `--json` stdout: `exit_code`, `turns_used`, `turn_budget`, token counts, `citations`, `evidence_urls`, `verification` (bullet grounding counts, when present), `replay_misses` (replay misses, or live fetches in `replay-through`), `wall_ms`, and the rendered answer. `eval judge` adds `judge` (nugget flags, the three recalls, SimpleQA class) and `claims` (one verdict per bullet).

Recall, per run (a mean over runs and goals in `summary` and `compare`):

| metric | meaning |
|---|---|
| `core_recall` | supported `core` nuggets / `core` nuggets. The headline: it measures what the question asks for. |
| `supporting_recall` | supported `supporting` nuggets / `supporting` nuggets; reported beside the headline, undefined for a goal with none. A better answer adds detail, so this moves with verbosity as well as quality. |
| `nugget_recall` | supported nuggets / all nuggets. Kept as the all-nugget mean; it depends on how many supporting nuggets the golden set holds. |

## Judge

```sh
cargo run --example eval -- judge base.json [--model gpt-5.6-sol] [--fixtures target/eval/fixtures]
```

One gateway call per answered run, with the same gateway env and a pinned model (default `gpt-5.6-sol`, a different family from the agent's `claude-*`), recorded as `judge_model` in the results file. It returns:

- per nugget: supported or not (same fact, same specific values); the recalls below come from these flags and the scope of each nugget in the golden file;
- a SimpleQA class: `CORRECT`, `INCORRECT` (contradicts a nugget, or the core claim is wrong) or `NOT_ATTEMPTED`.

Runs that exited non-zero are `NOT_ATTEMPTED` without a call. Judging is resumable and refuses a file already judged by another model.

### Claim pass

Recall asks whether the answer states the golden facts. The claim pass asks the converse: is every bullet true to the pages it cites? The Harness trusts each cited bullet, and the grounding check (`verification`) compares quotes and tokens, not claims, so it cannot stand in for this.

After the nugget grade, each answered run gets one more call to the same pinned judge model (a second only when the reply does not parse):

1. The rendered answer is split into bullets: the `-` lines under the `##` themes, before `Sources:`. The summary paragraph is not a bullet. The `(quote: "…")` suffix is cut off the bullet text, and the links in the bullet are its citations.
2. Each link is matched, by normalized URL (`dedup_key`, on the requested or the final URL), to a page the run fetched, read from `<fixtures>/<goal id>/pages.jsonl` (`--fixtures`, the dir the run was recorded into or replayed from; `replay-through` appends its live fetches to that same file). The pages are sent whole; a page over 150k characters, or a call over 500k, is cut and marked.
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

Results files carry a format version (`format: 2` since golden v2). `summary`, `compare` and `judge` reject a file of another format with an error naming both versions: nuggets judged against golden v1 are not comparable to v2 scores, so a v1 file is judged again from its recordings (`eval judge` on a copy with `format` set to 2 and the `judge` and `claims` fields removed), never read as is.

`summary` prints one Markdown row per category: exit-0 share, SimpleQA shares, mean core, supporting and nugget recall, pooled exact-quote share, and means of citations, turns, tokens and wall time. When any run has claims, a second table follows with the claim shares per category, pooled over bullets.

`compare` reports `core_recall`, `supporting_recall` and `nugget_recall` among its metrics. It pairs by goal: per metric, each goal in both files gives `d = mean(B over repeats) - mean(A over repeats)`, and the report is the mean of `d` with a two-sided 95% Student-t interval. A difference whose interval contains 0 is noise at this sample size.

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

- `docs/eval/baseline-2026-10-07.json`: the whole golden set, K=1, `record` mode at `46956ca` (after #116-#120), `claude-haiku-4.5`. Judged against golden v2 by `gpt-5.6-sol` with `--fixtures` on the recorded tapes, so it carries the nugget flags, the three recalls and the claim verdicts (`format: 2`). Compare new changes against this file. Its numbers: 84% correct, core recall 0.85, supporting recall 0.44, nugget recall 0.66, claim precision 68% (12% of bullets uncited).
- `docs/eval/baseline-2026-10-05.json`: the same at `80b1040`, judged against golden v1 (`format: 1`). Kept as history only: `summary` and `compare` reject it. Its v1 nugget recall (0.52) is not comparable to the v2 recalls above.

To judge an older record against a newer golden set, copy its results file, set `format` to the current version, drop every row's `judge` and `claims`, and run `eval judge <copy> --fixtures <the tapes it was recorded into>`.
