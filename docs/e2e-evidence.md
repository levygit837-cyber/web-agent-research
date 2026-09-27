# E2E validation evidence: Web Search with real tools (#33)

Verdict: **PASS**. On 2026-09-27 against `main` @ `44a5995`, a live CLI run went to a real LLM over the native Anthropic Messages API, fetched real pages, and returned a Synthesis in which every citation is a fetched page. A fresh Harness agent that was given only `docs/harness.md` called the tool and parsed the JSON.

This replaces the #16 PASS-WITH-GAP evidence, which ran against canned registry stubs (`example.com/obscura`, #25/#26).

## Setup

- LLM: local `kiro-backend` container (Kiro account manager, LLM proxy on `:5580`, native `/v1/messages`, no key check), model `claude-haiku-4.5`.
- `.env` (git-ignored):

  ```sh
  GATEWAY_API_FORMAT=anthropic
  GATEWAY_BASE_URL=http://localhost:5580/v1
  GATEWAY_API_KEY=local
  GATEWAY_MODEL=claude-haiku-4.5
  GATEWAY_TIMEOUT_SECS=120
  ```

- Fetch: reqwest + htmd static path; Obscura is installed for the fallback, but no page needed it.

## Passing run (Run A)

```sh
set -a; . ./.env; set +a
./target/release/web-agent-research research \
  "What is the Rust crate htmd and what does it do? Read https://docs.rs/htmd/latest/htmd/ and https://github.com/letmutex/htmd" \
  --json --size small --max-turns 8 --session-id e2e-33-haiku --session-out /tmp/war-e2e-33.jsonl
```

- Exit `0` in 7.7 s wall, `turns_used=2`: turn 1 made two parallel `fetch` calls, turn 2 wrote the answer.
- `evidence_urls`: `https://docs.rs/htmd/latest/htmd/`, `https://github.com/letmutex/htmd`.
- Session JSONL: header + 2 turn rows. Turn 1 Evidence is `docs.rs` (1,026 chars) and `github.com` (4,023 chars), both `fetch_path: "static"`.
- `synthesis.citations`: `https://github.com/letmutex/htmd`. **0 citations point at unfetched pages.**
- Themes: "What is htmd", "Core functionality".

Summary excerpt:

> htmd is a Rust crate that converts HTML to Markdown. It provides a high-level API through the `HtmlToMarkdown` struct and …

## Harness acceptance (Run B)

A fresh subagent was told to read only `docs/harness.md` (no source, no README). It loaded `.env` as the doc says and ran:

```sh
web-agent-research research "What does the Rust crate htmd do? Read https://github.com/letmutex/htmd" --json --size small
```

It got exit `0` after 3 turns. It parsed `synthesis.summary`, `synthesis.citations[].url`, and `evidence_urls`, and confirmed every citation is in `evidence_urls`. It reported no unclear or wrong spots in the doc.

## Findings fixed on the way

- Session rows stored `fetch_path: null` for every Evidence (#50).
- The model cited docs.rs sub-pages it only saw as links inside fetched markdown: 6 citations vs 2 fetched pages. Citations are now filtered against the run's Evidence (#51).
- The Anthropic wire could send whitespace-only text blocks, which the API rejects with 400 (#49).

## Known gaps

- **Search engines blocked from this IP**: DuckDuckGo HTML returns its anomaly page, and Startpage returns a proof-of-work challenge page that the parser does not recognize. Both yield zero Hits, so the run above passes URLs in the goal and exercises `fetch` only. A goal without URLs finishes with exit `0` and an "I found nothing" Synthesis.
- **Cache accounting through kiro-backend**: it reports `input_tokens: 0` and no cache fields, so `prompt_tokens`/`cached_prompt_tokens` read `0` in this run. The mapping is covered by hermetic tests against Anthropic-shaped responses.
- **Thinking through kiro-backend**: a `thinking` request to `claude-haiku-4.5` hung for 180 s without a response, so this run leaves `GATEWAY_THINKING_BUDGET` unset.
