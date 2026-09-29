# Free search engine evaluation (#65)

Measured, not assumed: which free/keyless/browserless search engines are worth adding after
DuckDuckGo + Startpage (the two already in `src/web/search/`). Research and measurement only —
**no engine code was added to `src/` in this PR**. That is issue #66's job; this issue picks the
priority order and hands #66 fixtures to build against.

> All times UTC, captured 2026-09-28. Header profile for every non-Wikipedia probe matches
> `apply_browser_headers` in `src/web/search/mod.rs` verbatim (desktop Chrome 126 UA + Sec-CH-UA/
> Sec-Fetch-*). Wikipedia used an honest, non-impersonating UA per Wikimedia policy (see its
> section). `[INFERENCE]` marks reasoning beyond a literal observed fact.

## Executive summary

- **Keep, no caveats**: Yahoo HTML, Brave HTML.
- **Keep, with one mandatory implementation detail**: Bing HTML — it silently geo/language-
  localizes on client IP with zero explicit signal, and got **0/5 relevant top-5 results** on the
  reference query until an explicit `mkt=en-US&setlang=en` was added, after which it jumped to
  **5/5**. Ship the param, not optional.
- **Maybe**: Yandex (works once three extra params are added — the documented-looking bare
  endpoint returns an empty-but-200 trap), Wikipedia REST (excellent transport, but 4/10 fixed
  queries returned zero pages — it's a narrow supplementary source, not general web search),
  Marginalia (the shared `public` key is dead on arrival this session; a free dedicated key is one
  email away and would flip this to keep), Mwmbl (zero cost, zero risk, but weak relevance).
  Chatnoir has been re-classified: the previous "Chatnoir" candidate on this project's radar is
  in fact a fixed academic IR-benchmark index, not a live web search engine — see its section.
- **Drop**: Mojeek (ALTCHA gates every request that reaches it, and the domain was outright
  TCP-unreachable for ~25 minutes this session from this network — as limiting as Startpage, the
  task's stated bar), Qwant (DataDome 403 on the very first request, no burst allowance at all),
  Google `/wml` (403 even with the documented Nokia-UA workaround — this route is dead), Chatnoir
  (frozen academic snapshots, not a web index — architecturally wrong tool). Startpage stays
  dropped (Anubis PoW difficulty 6, confirmed again by this session's single probe).

## Methodology

### Fixed query set

10 queries spanning the categories the task named. IDs are stable across every section below.

| ID | Category | Query |
|---|---|---|
| Q1 | Rust crate docs | `tokio rust crate documentation` |
| Q2 | Rust crate docs | `serde_json rust crate documentation` |
| Q3 | API reference | `reqwest ClientBuilder timeout method rust` |
| Q4 | Exact error message | `error[E0502]: cannot borrow as mutable because it is also borrowed as immutable` |
| Q5 | Recent news | `Rust 1.98.0 release notes` |
| Q6 | How-to | `how to iterate over a HashMap in Rust` |
| Q7 | Non-English (pt-BR) | `como fazer um servidor HTTP simples em Rust` |
| Q8 | Ambiguous term | `cargo` |
| Q9 | Ambiguous term | `rust` |
| Q10 | Long natural-language question | `What is the difference between async and sync HTTP clients in Rust and when should I use each one?` |

### Relevance labeling (hand-labeled, disclosed)

Single-rater (this task), judged on URL/title/domain from the raw captured fixtures. Criterion
per query, applied identically across every engine:

- **Q1/Q2** (crate docs): relevant = a page about the named crate itself (its `docs.rs`/`crates.io`
  entry, GitHub repo, official site, or a tutorial specifically about that crate). Same-named
  unrelated brands/places (e.g. "Tokio Marine" insurance, "Tokyo" the city) are **not** relevant.
- **Q4** (error message): relevant = any page discussing that specific Rust borrow-checker error
  (Stack Overflow, blog posts, the Rust book, GitHub issues about it).
- **Q5** (news): relevant = a page specifically about Rust 1.98.0 (release notes, announcement,
  changelog). Generic Rust-language pages without version specificity are not relevant.
- **Q6** (how-to): relevant = a page about iterating HashMaps, in Rust specifically.
- **Q9** (ambiguous "rust"): relevant = the programming-language sense (rust-lang.org, its docs,
  its GitHub, its Wikipedia page) — a reasonable proxy for "what would an agent doing Rust
  research want," not the literal-oxidation or video-game senses.

Precision@K = (relevant in top K) / K. Full URL lists and labels are reproducible from the raw
fixtures under `fixtures/`; the labeling logic itself is not checked into a script (throwaway,
per the task's "throwaway Rust or curl harness" instruction — deleted after use).

### Live-traffic budget accounting

Shared residential IP, per the wave's binding rules. Totals below are exact counts from saved
response files / request logs, not estimates.

| Engine | Requests sent | Cap | Notes |
|---|---|---|---|
| DuckDuckGo | 6 (me) + 6 (TransportDecision) = **12/12** | 12 shared | Coordinated live via IRC; both sides confirmed done. |
| Startpage | 1 | 1 (confirmation only) | Per task instructions. |
| Brave | 9 | 15 | Stopped after 2 consecutive 429s (Q8, Q9). |
| Bing | 11 | 15 | 10-query pass + 1 `mkt=en-US` correction probe. Zero blocks. |
| Yahoo | 10 | 15 | Zero blocks after switching to HTTP/1.1 (see its section). |
| Yandex | 15 (10 wrong-endpoint + 5 corrected) | 15 | Wrong-endpoint pass came from a helper before the endpoint-shape bug was found. |
| Mojeek | ~13 across 2 agents | 15 | Mostly TCP timeouts, not HTTP responses; see its section. |
| Qwant | 2 | 15 | Stopped after 2 consecutive 403s (DataDome), as required. |
| Marginalia (new/public key) | 3 | 15 | Stopped after repeated 429; never got a non-blocked response. |
| Marginalia (old/deprecated endpoint) | 4 | 15 | 1 success, 3 network timeouts (not rate-limit responses). |
| Wikipedia (EN) | 10 | 15 | |
| Wikipedia (PT) | 2 | 15 | Comparison-only, per task item 3 wasn't required but recon suggested it. |
| ChatNoir | 12 | 15 | Exploratory: index variants, single-vs-multi-word, `bm25` vs `default`. |
| Mwmbl | 7 (5 succeeded, 2 timed out) | 15 | |
| Google `/wml` | 1 | 15 | Immediate 403; stopped, no point spending more. |

No Anubis/ALTCHA/CAPTCHA was solved or submitted at any point. Two subagents were spawned
(`Tier1Probes`: Brave/Bing/Yahoo; `Tier2Probes`: Mojeek/Marginalia/Wikipedia/Yandex/Qwant), both
told not to spawn further subagents, both respecting the same per-engine pacing/caps.

---

## Comparison table

Every metric the task named. "—" = not applicable or not testable given how the engine blocked.

| Engine | Reachability | Block type / signal | Burst budget observed | Results/page | Paging (untested unless noted) | URL quality | Dup rows (Q1) | Dead links (sample) | P@5 (avg across tested Q) | Jaccard vs DDG (Q1) | Latency p50/p95 | Bytes/page (Q1) | Parse | robots.txt / ToS |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **DuckDuckGo** (baseline) | 6/6 (100%) | `anomaly-modal` body marker | — | 10 | `s`/`vqd` continuation (existing) | Wrapped (`uddg=`), unwrapped in repo | 0 | 0 dead / 6 sampled | 0.76 | 1.00 (self) | 1.074s / 1.298s | 28,453 B | Regex (existing) | Disallow `/html`,`/lite`,`*` |
| **Startpage** | 1/1 HTTP-wise, 0/1 useful | Anubis PoW, `id="anubis_challenge"`, status 200 | — (wall on every request) | 0 | — | — | — | — | — | — | 1.066s (to the wall) | 22,105 B | Existing `is_startpage_challenge` | Disallow `/sp/`; support KB: "billions of requests... we filter out unwanted searches" |
| **Brave HTML** | 7/9 (78%) | HTTP 429, CloudFront edge (`x-cache: Error from cloudfront`), no unique body marker | ~7 clean before 2 consecutive 429s | 18 | `offset=N` (untested live — blocked before reaching the test) | Direct, unwrapped | 0 | 0 dead / 5 sampled | 0.80 | 0.167 (14 unique) | 1.197s / 1.254s | 46,406 B | CSS classes (Svelte hash, fragile) or embedded JSON (SearXNG's approach) | Disallow `/search` for `*` |
| **Bing HTML** (no `mkt`) | 10/10 (100%) | None fired | — | 10 | JS-required (SearXNG docstring, not tested) | Wrapped (`ck/a?...&u=a1<base64url>`) | 0 | 0 dead / 6 sampled | **0.00** | **0.00** | 0.351s / 0.649s | 124,461 B | XPath, stable-ish | Disallow `/search` for `*`; MS Services Agreement names "impermissible scraping" |
| **Bing HTML** (`mkt=en-US`) | 1/1 (100%) | None | — | 6 | same as above | same wrapper | 0 | — | **1.00** | **0.60** (0 unique) | 0.377s (n=1) | 121,030 B | same | same |
| **Yahoo HTML** | 10/10 (100%) | None (YBV cookie-chain is the *mechanism*, not a block) | — | 7–8 | `b`/`pz` params (untested live) | Wrapped (`r.search.yahoo.com/...RU=<url>/RS=`) | 0 | 0 dead / 6 sampled | 0.76 | 0.70 (0 unique) | 0.968s / 2.199s | 154,701 B (cold, incl. redirect chain) | XPath, stable-ish | Disallow `/search` for `*`; blocks ~40 named AI/agent crawlers by UA |
| **Yandex** (corrected endpoint) | 5/5 (100%) | `x-yandex-captcha` header (absent throughout) | — | 15 | `p=` param (untested live) | Direct, unwrapped | 0 | 0 dead / 4 sampled | 0.53 | 0.231 (3 unique) | 1.830s / 2.053s | 77,149 B | XPath, matches SearXNG exactly | Disallow `/search` for `*`; User Agreement: softer "may prohibit" |
| **Yandex** (naive endpoint) | 10/10 HTTP-wise, 0/10 useful | None — endpoint-shape bug, not a block | — | 0 | — | — | — | — | 0.00 | 0.00 | 0.833s (median) | 6,536 B | — | same |
| **Mojeek** | ~1/13 useful | ALTCHA CAPTCHA (title="Captcha") on the one HTTP response received; TCP-unreachable (curl error 28) the rest of the session | Unmeasurable — mostly no TCP connection at all | 0 | — | — | — | — | — | — | 1.040s (to the wall, n=1) | 5,519 B | — | ToS explicitly bans scraping (prior recon) |
| **Qwant (internal API)** | 0/2 (0%) | HTTP 403, DataDome CAPTCHA-delivery redirect | 0 — blocked on request #1 | 0 | — | — | — | — | — | — | 0.692s (to the wall) | 553 B | JSON (would be, if reachable) | Query-string rule + ~60 named AI-bot UAs disallowed |
| **Marginalia (public key, new API)** | 0/3 (0%) | HTTP 429, plain-text `QPM Limit Exceeded` / `Daily Limit Exceeded`, `API-Remaining-Daily-Capacity: 0` | 0 — blocked on request #1 | 0 | `page=`,`count=` (documented, untested) | — | — | — | — | — | 0.6–0.9s (to the wall) | 18–20 B | JSON | Disallow `/search` (HTML UI only); API is the sanctioned path |
| **Marginalia (old/deprecated endpoint)** | 1/4 (25%) | 3× network timeout (not a rate-limit response) | — | 10 | `page=` (documented) | Direct, unwrapped | 0 | 0 dead / 5 sampled | 0.60 (P@10 0.40) | — | 0.72–1.02s (n≈2) | 4,736 B | JSON | same |
| **Wikipedia REST (EN)** | 10/10 (100%) | None | — | 5–10 (4/10 queries: 0) | `offset` native (not scrape-paging) | Direct (wiki page keys) | 0 | 0 dead / 2 sampled | 0.00 (tech queries) | — | 0.548s / 0.764s | 3,222 B | Pure JSON | Pro-automation; honest UA **required**, browser-impersonating UA "may be blocked or malicious" |
| **ChatNoir** | 12/12 HTTP-wise, ~0/12 useful | None (HTTP-level) — relevance failure, not a block | — | 0–10 | `from`/`size` (documented) | Direct | 0 | 0 dead / 0 sampled | 0.00 | — | 0.6–4.7s (highly variable) | 553–10,266 B | Pure JSON | Content-signal opt-in scheme; no explicit `search=no` found |
| **Mwmbl** | 5/7 (71%) | 2× network timeout on complex queries (not a rate-limit response) | — | 10–18 | `number_of_results` shown, no page param tested | Direct | 0 | 0 dead / 3 sampled | 0.00 | — | 0.55–8.8s (highly variable) | 3,982–6,990 B | Pure JSON | No robots.txt (404); open-source, AGPL-3.0, stated ethical-search mission |
| **Google `/wml`** | 0/1 (0%) | HTTP 403, standard Google "sorry" page, Nokia UA workaround already applied | 0 | 0 | — | — | — | — | — | — | 1.046s (to the wall) | 1,642 B | XML/XPath (would be) | Disallow `/wml*` explicitly |

---

## Per-engine evidence

### Brave HTML — KEEP

`search.brave.com/search?q=<query>&source=web`, cookies `safesearch=off; useLocation=0;
summarizer=0; country=us; ui_lang=en-us` (matching SearXNG's own `brave.py::request`).

- **Reachability**: Q1–Q7 all `200`, first at 2026-09-28T15:53:59Z (46,406 B), last clean at
  15:55:01Z. Q8 (`cargo`) and Q9 (`rust`) both `429` at 15:55:12Z/15:55:22Z — two in a row, so the
  probe stopped per the shared rule. The 429 body (73,998 B both times) is byte-identical to
  Brave's normal SvelteKit app shell, not a themed "blocked" page — **detection must be
  status-code-based, no body marker exists**. Headers show `x-cache: Error from cloudfront`,
  `via: 1.1 ...cloudfront.net`, no `Retry-After`.
- **Results/page**: 18 organic `div.result-wrapper` rows on Q1, direct unwrapped hrefs
  (`https://docs.rs/tokio`, `https://crates.io/crates/tokio`, ...).
- **Relevance**: Q1 P@5 = 1.00, Q4 P@5 = 1.00, Q5 P@5 = 0.60, Q6 P@5 = 0.60 → avg 0.80.
- **Cohesion**: Jaccard 0.167 vs DDG on Q1 (4 shared / 24 union), 14 URLs unique to Brave
  (`crates.io/crates/tokio-{console,core,io,stream,util}`, `docs.rs/crate/tokio/0.1.x`, ...) — real
  incremental breadth, not just overlap.
- **Parse robustness**: organic anchor class is `svelte-14r20fy l1` — a Svelte content hash that
  **will** break on Brave redeploys. SearXNG's own `brave.py` instead scrapes an embedded
  `data: [{...}]` JSON blob from a `<script>` tag; that's the more durable path if this is added.
- **robots.txt**: `Disallow: /search` for `User-agent: *` (live-read 2026-09-28).
- **Verdict**: **keep**. Good relevance, real breadth over DDG, no CAPTCHA — just a firm rate
  limit after roughly 7 clean requests, matching prior recon's "~9 pages, most likely flagged."

### Bing HTML — KEEP, with a mandatory param

`www.bing.com/search?q=<query>` (no `mkt`) vs `...&mkt=en-US&setlang=en` (corrected).

- **Reachability**: 10/10 `200` on the uncorrected pass (first 15:55:55Z, 124,461 B; last
  15:57:21Z), zero blocks, fastest of any live-scraped engine this session (p50 0.351s).
- **Critical finding**: with **no explicit market param**, Bing auto-localized to `mkt=pt-BR`
  purely from this Brazilian residential IP. Q1 (`tokio rust crate documentation`) came back with
  Tokio Marine (a Brazilian insurance company) and Tokyo/Wikipedia hits — **0 of the top 10 were
  about the Rust crate**. Adding `&mkt=en-US&setlang=en` (probed once, 16:52:37Z, 121,030 B) fixed
  it completely: top 6 results are `tokio.rs`, `hid-io.github.io/tokio`, `docs.rs/crate/tokio`,
  `github.com/tokio-rs/tokio` — P@5 1.00, Jaccard 0.60 vs DDG. This is not a cosmetic difference;
  it's the difference between a useless and a good engine for this exact IP.
- **URL quality**: every organic `<h2><a>` href is wrapped through
  `bing.com/ck/a?...&u=a1<base64url-no-padding>`; unwrap by base64url-decoding the `u=a1...`
  segment (see fixture for the exact shape — distinct from DDG's `uddg=` percent-encoding scheme).
- **robots.txt**: `Disallow: /search` for `*` (live-read). Microsoft Services Agreement §3/§14f
  names "impermissible scraping" alongside jailbreak attempts.
- **Verdict**: **keep**, but `mkt`/`setlang` (or an equivalent explicit locale signal derived from
  the Research's actual language, not left to IP geolocation) is not optional — ship it in the
  same commit that wires Bing in, not as a follow-up.

### Yahoo HTML — KEEP

`search.yahoo.com/search?p=<query>`, `YBV` cookie chain replayed across a persistent cookie jar.

- **Reachability**: 10/10 `200` (application-level) after switching curl to `--http1.1` — the
  first attempt over HTTP/2 hit a transport-level "HTTP2 framing layer" error (curl code 16, no
  real HTTP response at all) that a throwaway Rust binary using **this repo's exact `reqwest`
  feature flags** (`default-features=false, features=["json","rustls-tls"]`) confirmed
  independently: `reqwest::Client::new()` negotiated `HTTP/1.1` on its own and got a clean `200`
  with `algo-sr` results present, 144,308 B, 3.02s. **This means the repo's existing reqwest
  client already avoids the h2-framing trap** — nothing to fix, just confirming it.
- **YBV chain**: first request (15:58:11Z) took 2 redirect hops — hop 1 sets a 60-second tracking
  cookie (`YBV=v0.1...`), hop 2 sets the real 24-hour cache cookie (`YBV=v0.2...`), hop 3 returns
  the SERP. Every subsequent query in the same session (Q2–Q10, same jar) went straight to `200`
  with **zero redirects** — the persisted `v0.2` cookie is sufficient for the full 24h window,
  exactly matching SearXNG's `yahoo.py` (`CACHE.set("YBV", ybv, expire=86400)` only on `v0.2`
  values).
- **Relevance**: Q1 P@5 1.00, Q4 P@5 1.00, Q5 P@5 0.60, Q6 P@5 0.60 → avg 0.76.
- **Cohesion**: Jaccard **0.70** vs DDG on Q1 (7 of 10 DDG URLs also in Yahoo's 7) — the highest
  overlap of any engine tested, meaning Yahoo is highly reliable but contributes little unique
  breadth over DDG specifically (0 unique URLs on Q1).
- **URL quality**: wrapped via `r.search.yahoo.com/_ylt=.../RU=<url-encoded target>/RS=...`;
  unwrap by taking everything from the first `http` after `/RU=` up to the nearest `/RS` or `/RK`
  (ported directly from SearXNG's `yahoo.py::parse_url` — same algorithm, different constant).
- **robots.txt**: `Disallow: /search` for `*`; separately full-site-blocks ~40 named AI/agent
  crawlers by exact UA (`GPTBot`, `ClaudeBot`, `PerplexityBot`, ...).
- **Verdict**: **keep**. Zero friction, well-understood cookie mechanics (same shape as this
  repo's existing DDG/Startpage form-token patterns), good relevance. Lower priority than Brave
  for *breadth* specifically, but the most reliable resilience hedge if DDG/Startpage are both
  walled on a given IP.

### Yandex — MAYBE

`yandex.com/search/site/?text=<query>&lang=en&tmpl_version=releases&web=1&frame=1&searchid=3131712`
(the SearXNG-documented full param set) vs the bare `?text=<query>&web=1` shape a naive port
might try.

- **Endpoint-shape trap, found live**: a first pass (10 queries, by a helper) against the bare
  `?text=<q>&web=1` URL got **10/10 HTTP 200 with zero results every time** — the page title
  literally reads *"There are no search results for `<query>`"*. This is Yandex's per-site-search
  widget shape, not general web search, and it is **structurally incapable** of returning results
  regardless of query or blocking. No `x-yandex-captcha` header fired either — a status/header
  check alone would have missed this entirely. Adding `tmpl_version=releases&frame=1&searchid=
  3131712` (3 extra params, all present in SearXNG's `yandex.py::request` but easy to drop) fixed
  it: same Q1, 15 real results (`docs.rs/tokio`, `github.com/tokio-rs/tokio`, ...), 77,149 B.
- **Relevance**: Q1 P@5 1.00, Q4 P@5 0.60, Q9 (`rust` alone) P@5 **0.00** — top 5 were
  `google.github.io/comprehensive-rust`, a Medium post, a LinkedIn post, a YouTube video, and a
  Steam community page, none of them `rust-lang.org` or its docs. Average across the 3 graded
  queries: 0.53 — inconsistent, weighted toward tutorial/social content over official docs on
  bare terms.
- **Cohesion**: Jaccard 0.231 vs DDG on Q1, 3 unique URLs (`doc.servo.org/tokio`,
  `rust.codeguides.io/tokio`, one more) — modest but real incremental value.
- **robots.txt**: `Disallow: /search` for `*` (live-read, 842-line file). User Agreement §3.1 uses
  softer, discretionary language ("Yandex **may** prohibit automatic requests") than Yahoo/
  Mojeek's flat bans.
- **Verdict**: **maybe**. Works cleanly once correctly shaped, no CAPTCHA fired this session, but
  the empty-but-200 trap for the "obvious" endpoint shape is a real correctness risk for whoever
  implements #66, and relevance is the weakest of the three keep-tier HTML scrapers. Worth adding
  after Brave/Bing/Yahoo are stable, with the exact 6-param shape documented prominently (see
  fixture `yandex/Q1-WRONG-ENDPOINT-empty-200.html` as the regression case to guard against).

### Mojeek — DROP

`mojeek.com/search?q=<query>`.

- **Reachability**: the domain was **TCP-connection-timing-out** (curl error 28, `Failed to
  connect... after ~10000ms`) for roughly 25 minutes this session, reproduced independently by
  two separate agents (main + a helper), both forcing IPv4, both trying bare and `www.` hostnames,
  ~13 attempts total. It then briefly came up: **the one HTTP response received (16:14:14Z,
  200 OK, 5,519 B) was itself an ALTCHA CAPTCHA page** — `<title>Captcha</title>`,
  `<div class="captcha-wrap"><p>JavaScript is required to complete this challenge...`. Five more
  queries sent immediately after all timed out again the same way.
- **Why this meets the task's Startpage-equivalent bar**: status 200 on a wall (exactly like
  Startpage's Anubis), a hard JS-execution requirement stated in the challenge page itself
  (exactly like Startpage), and on top of that, outright unreachable for the majority of this
  session — worse connectivity than Startpage, which at least always answered.
- **[INFERENCE]** Whether the TCP-timeout pattern is IP/ASN-reputation-based or unrelated network
  flakiness on this specific residential connection couldn't be disambiguated within this
  session's probe budget — but it doesn't change the verdict, since the ALTCHA wall fires
  regardless of whether the TCP connection even succeeds.
- **robots.txt**: not fetched live (domain unreachable); prior recon already quotes Mojeek's own
  ToS banning scraping verbatim.
- **Verdict**: **drop**. Matches the task's explicit bar ("drop anything as limiting as
  Startpage") on two independent axes.

### Qwant (internal API) — DROP

`api.qwant.com/v3/search/web?q=<query>&count=10&locale=en_US&offset=0&device=desktop`.

- **Reachability**: 0/2. Both Q1 (15:58:00Z) and Q2 (15:58:20Z) — the very **first and second**
  requests of the session — returned HTTP `403` with a JSON body:
  `{"url":"https://geo.captcha-delivery.com/captcha/?..."}`, a DataDome CAPTCHA-delivery redirect.
  No burst allowance observed at all; the block fired before any query-specific behavior
  (including the SearXNG-issue-#6358 "fabricated lorem-ipsum results" failure mode) could even be
  tested, because the request never reached Qwant's own backend.
- **robots.txt**: full site block of ~60 named AI/agent-crawler UAs plus a query-string rule
  (`Disallow: /?*q=*`) for `User-agent: *` — the second-most aggressive UA-naming policy surveyed,
  after Yahoo.
- **Verdict**: **drop**. Zero burst budget, DataDome fires on request #1, and prior recon already
  documents a harsh 24h suspension once tripped plus a documented silent-fabrication failure mode
  this session couldn't even reach to verify.

### Marginalia API — MAYBE (contingent on a dedicated key)

`api2.marginalia-search.com/search?query=<query>&count=10`, `API-Key: public`.

- **Reachability (current/documented endpoint, shared `public` key)**: 0/3. Every attempt this
  session — including the very first, a bare shape-check call before any real query — hit HTTP
  `429`. Two distinct messages observed across the session: `QPM Limit Exceeded` (early) and
  `Daily Limit Exceeded` with header `API-Remaining-Daily-Capacity: 0` (later). **[INFERENCE]**
  The daily-capacity-zero signal suggests the shared key's global quota may already be exhausted
  by other consumers, independent of this session's own request rate — the docs themselves warn
  the `public` key "often hits a rate limit."
- **Reachability (deprecated endpoint, same `public` key)**: `api.marginalia.nu/public/search/
  <query>?count=N` — the docs explicitly call this deprecated but say it "will work as long as
  the project does." 1 of 4 attempts succeeded (15:58:19Z, 200, 10 real results, 4,736 B); the
  other 3 were network timeouts (curl error 28), not rate-limit responses — likely unrelated
  connectivity flakiness (the same session saw similar transient timeouts against Mwmbl and
  Mojeek). When it answered, relevance was decent: Q1 P@5 = 0.60, P@10 = 0.40
  (`readrust.net/crates`, `tokio.rs/tokio/glossary`, `tokio.rs/blog/2017-09-tokio-reform`, ...).
- **robots.txt**: `Disallow: /search` etc. for the HTML UI only — the API is the documented,
  ToS-sanctioned path, not a workaround.
- **Verdict**: **maybe**. The shared `public` key is unusable as-is this session (rate-limited
  before we could even measure it properly). The docs offer a **free** non-commercial key via a
  one-line email (`contact@marginalia-search.com`) with no other cost — prior recon already flags
  this as "a legitimate, ToS-clean, zero-cost addition." Getting that key is a human action, not
  something this task can do; see open questions.

### Wikipedia REST API — MAYBE, narrow scope

`en.wikipedia.org/w/rest.php/v1/search/page?q=<query>&limit=10`. Honest, non-impersonating UA
(`web-agent-research-recon/0.1 (+https://github.com/...; research-only, issue #65)`) used for
these requests specifically — **the opposite of every other engine's browser-impersonating UA**,
per the Wikimedia Foundation User-Agent Policy ("bot-like behavior with a browser's user agent
will be assumed malicious").

- **Reachability**: 10/10 `200`, fast (p50 0.548s, p95 0.764s), smallest bytes/page of any engine
  tested (3,222 B on Q1).
- **Relevance is the real limiter, not reachability**: 4 of the 10 fixed queries returned
  **zero pages** despite `200` — Q3 (API method name `reqwest ClientBuilder timeout`), Q4 (exact
  compiler error string), Q7 (pt-BR query, tested against *both* `en.wikipedia.org` and
  `pt.wikipedia.org` — pt.wikipedia returned non-empty but irrelevant results, "C Sharp" and
  "Mozilla Firefox," not Rust-HTTP-server content), and Q10 (long natural-language question). Q1
  itself returned real, on-topic results (`Rust_(programming_language)`,
  `Outline_of_the_Rust_programming_language`, ...) but also two clearly-wrong matches
  (`Glauber_Costa`, `ETamil` — incidental keyword overlap).
- **Implementation note**: because this is the one engine where impersonation is a documented red
  flag by the origin, wiring it in means a **second, distinct request path** — it cannot share
  `apply_browser_headers` with every other leg. That's a design decision for #66, flagged below.
- **robots.txt**: `Allow: /w/rest.php/site/v1/sitemap`, `Disallow: /w/`, `/api/` for `*` — the
  REST search endpoint (`/w/rest.php/v1/search/page`) isn't in an explicit `Allow`, but Wikipedia's
  own API:Etiquette page treats the REST/Action APIs as the sanctioned access path (contrasted
  with disallowing scraping of rendered `/wiki/` pages), consistent with prior recon's "one
  affirmatively pro-automation engine" framing.
- **Verdict**: **maybe**. Zero risk, zero cost, but not a general web-search substitute — 40% of
  the fixed query set came back empty. Best framed as a narrow supplementary source for
  encyclopedic/ambiguous-term queries (Q8/Q9-shaped), not a priority general-purpose addition.

### ChatNoir — DROP (reclassified: not a live web index)

`chatnoir.eu/api/v1/_search`, shared default API key (`LTmnNLQeQvBlNjwWeuNxz1vdya3HpSzN` — this
is the official Python client's own published `DEFAULT_API_KEY` constant, baked into the
open-source `chatnoir-api` package; not a secret this task generated or extracted).

- **What it actually indexes**: fixed academic IR-benchmark snapshots — ClueWeb09 (2009),
  ClueWeb12, ClueWeb22, four MS MARCO variants, TREC-ToT 2024, LongEval-SCI, and
  `wows-owi-2025` (the newest, an OpenWebSearch.eu 2025 research crawl — still a frozen snapshot,
  not a continuously updated index). **This is a research-reproducibility tool, not a web search
  engine.**
- **Reachability**: 12/12 HTTP `200` across every variant tried (default index, explicit
  `clueweb22/b`, no-index-specified, `bm25` search method) — never blocked, but that's beside the
  point.
- **Relevance, confirmed empirically**: Q1 (`tokio rust crate documentation`, 4 words, default
  `search_method`) → `total_results: 2`, **0 returned** (below the result threshold). Dropping to
  the single word `tokio` → 2,689 matches, but the top 5 by score are all "Tokio Hotel" (the
  German pop band) and Tokyo-adjacent pages — zero about the Rust crate. Switching to
  `search_method: "bm25"` on the 4-word query made it *worse*, not better: top hits were an
  `npm` package page (`@swc/core`), a Debian testing-removal changelog, and a Fedora EPEL package
  announcement — generic keyword co-occurrence in a research corpus, not agent-relevant results.
  A separate solo-word `rust` probe returned PlayStation-game hits before any programming-language
  page.
- **robots.txt**: uses the new IETF content-signals draft (`search`/`ai-input`/`ai-train` yes/no
  per-signal), with **no explicit signal set for any of them** — per the file's own stated
  semantics, this "neither grants nor restricts" automated search use.
- **Verdict**: **drop**. Not a reachability or ToS problem — it's the wrong tool. An academic
  IR-benchmark index cannot substitute for live web search for an agent doing programming
  research, and no query-method combination tried changed that.

### Mwmbl — MAYBE (the "any free keyless engine" find)

`api.mwmbl.org/api/v2/search/?q=<query>`. Found via SearXNG's `mwmbl.py` engine
(`about = {"require_api_key": False}`); anonymous free tier is 1,000 req/month at 1 req/s,
IP-tied, no signup. (Ecosia, the other Omp-side free provider from prior recon, was **not**
live-probed: Omp's own implementation escalates to headless Chromium on any fetch failure, which
violates this project's no-browser constraint outright — not a fair "keyless fetch-only"
candidate to begin with.)

- **Reachability**: 5/7 succeeded (Q1, Q2, Q5, Q6, Q7); Q3 (API-method-name query) and Q4
  (exact-error-message query) both timed out at the 15s ceiling — no error status, just no
  response in time. **[INFERENCE]** More likely slow index lookups on unusual query shapes than a
  rate-limit block (no 429/503 was ever observed, and the successful queries all returned in
  under 1s).
- **Relevance**: Q1 P@5 = **0.00** — the top matches were `users.rust-lang.org` forum threads and
  repeated `gregoryszorc.com/docs/pyembed/*` version pages (a Python-Rust-binding project), because
  "crate" + "documentation" + "Rust" happened to co-occur there, not because Mwmbl found anything
  about the `tokio` crate specifically. Small, community-crawled index; genuinely useful hits
  exist but incidental keyword matches dominate the top of the list on this query shape.
- **robots.txt**: `api.mwmbl.org/robots.txt` and `mwmbl.org/robots.txt` both `404` — no
  machine-readable policy either way; the project's stated mission (open-source, AGPL-3.0,
  "the truly open source search engine," explicit no-ads/no-tracking framing) is the closest thing
  to a ToS stance, and it's unambiguously automation-friendly in spirit.
- **Verdict**: **maybe**. Free, keyless, zero legal/ethical friction, zero request-budget risk —
  but the index is too sparse and the ranking too keyword-incidental to be a reliable primary
  source. Fine as a low-priority supplementary fallback; not worth prioritizing.

### Google `/wml` — DROP (confirmed dead)

`google.com/wml/search?q=<query>&ie=utf8&oe=utf8`, Nokia feature-phone UA
(`Nokia6230/2.0 (05.50) Profile/MIDP-2.0 Configuration/CLDC-1.1`) — the exact documented SearXNG
workaround for Google's JS-gating of normal HTML results (per
[searxng/searxng#6359](https://github.com/searxng/searxng/issues/6359)).

- **Reachability**: 0/1. Single probe (15:53:18Z) returned HTTP `403`, standard Google
  robot-block page ("**403.** *That's an error.* Your client does not have permission to get URL
  `/search?q=...` from this server."), 1,642 B. Not a CAPTCHA redirect — a flat deny, even with
  the documented UA workaround already applied.
- **robots.txt**: `Disallow: /wml?`, `/wml/?`, `/wml/search?` — explicitly disallowed, on top of
  being functionally dead.
- **Verdict**: **drop**. Confirms the recon's finding that this stopgap is "eventually blocked
  too" (SearXNG's own #6359 thread, unresolved as of its last read). No further probe budget
  justified.

---

## Verdicts and priority order for #66

| Engine | Verdict | Reason (one line) |
|---|---|---|
| Yahoo HTML | **keep** | 100% reachable, well-understood cookie chain, good relevance, but high DDG overlap. |
| Brave HTML | **keep** | Good relevance and real breadth beyond DDG; rate-limits after ~7 requests, no CAPTCHA. |
| Bing HTML | **keep** (mandatory param) | Fastest and 100% reachable, but useless without explicit `mkt`/`setlang` — ship it always. |
| Yandex | **maybe** | Works once 3 extra params are added; the "obvious" shape is an empty-but-200 trap; inconsistent relevance. |
| Wikipedia REST | **maybe** | Zero-risk, zero-cost, but 40% of fixed queries return zero pages — narrow supplementary use only. |
| Marginalia API | **maybe** | Shared key dead this session; a free dedicated key (one email) would flip this to keep. |
| Mwmbl | **maybe** | Zero cost/risk, but weak relevance (incidental keyword matches). Low priority. |
| Mojeek | **drop** | ALTCHA on every reachable request + outright unreachable most of this session. |
| Qwant | **drop** | DataDome blocks the very first request; no burst allowance. |
| Google `/wml` | **drop** | 403 even with the documented UA workaround; route is dead. |
| ChatNoir | **drop** | Frozen academic IR-benchmark corpus, not a web index — wrong tool for the job. |
| Startpage | **drop** (reconfirmed) | Anubis PoW difficulty 6 on every request; this session's one probe reconfirms it. |

**Recommended build order for #66**: Yahoo → Brave → Bing (with mandatory `mkt`/`setlang`) as one
wave (all three are "keep," all three are plain HTML scrapes matching the existing DDG/Startpage
pattern in `src/web/search/`). Yandex next, once the exact 6-param query shape is documented as a
regression test (see the `yandex/Q1-WRONG-ENDPOINT-empty-200.html` fixture). Wikipedia REST after
that, as its own distinct request path (different UA, no shared `apply_browser_headers`) — treat
it as a new small module, not a fourth leg on the existing pattern. Marginalia only after a
dedicated key is obtained (human action, see open questions). Mwmbl last, if at all — lowest
relevance-per-engineering-effort ratio of the keep/maybe tier.

**Pagination depth**: **page 1 only for the initial #66 cut, across every engine.** Bing cannot
paginate without JS at all (confirmed via SearXNG's own docstring, not re-tested live). Brave's
`offset=N` and Yahoo's `b`/`pz` and Yandex's `p=` params are all documented in SearXNG but were
**not tested live this session** — the Brave pagination probe was planned but had to be skipped
because the engine hit its 2-consecutive-429 stop before reaching it, and testing the others
would have meant spending probe budget on a metric (paging depth) below reachability/relevance in
the task's own priority, given the tight per-engine caps. Recommend #66 test paging live, on a
fresh window, before shipping anything beyond page 1.

**Human decision (2026-09-29)**: #66 ships Brave HTML, Yahoo HTML, and Bing HTML only. Yandex,
Wikipedia REST, Marginalia, and Mwmbl stay documented here as "maybe", not wired in. Before
per-engine budgets are set, each kept engine gets measured request validation (concurrency,
minimum safe interval, burst budget, cooldown) and live pagination tests; whatever an engine
tolerates beyond the conservative defaults is used. The HTTP transport stays on `reqwest`
(#56); HTTP/2 (#60) stays open, waiting.

---

## Open questions for the human

1. **Bing's silent geo/language localization.** Should the repo hardcode `mkt=en-US&setlang=en`
   unconditionally, or derive it from the Research's actual detected language (this repo already
   handles a pt-BR query in Q7)? Getting this wrong silently degrades Bing to useless from any
   non-US IP — it's a correctness decision, not a style one.
2. **Marginalia's free dedicated key.** Emailing `contact@marginalia-search.com` for a free
   non-commercial key is a one-time human action outside this agent's authority (an external
   email on the project's behalf) — worth doing before #66, or should Marginalia stay parked on
   the shared `public` key (currently unusable) until someone gets to it?
3. **Wikipedia's separate request path.** Every other engine shares one `apply_browser_headers`
   helper. Wikipedia needs the opposite (an honest, descriptive UA) by the origin's own explicit
   policy. Is a second header-building helper acceptable, or should Wikipedia wait until there's a
   second "opt out of impersonation" engine to justify a shared seam?
4. **Live pagination testing.** Should #66 spend a fresh probe budget testing Brave `offset=N`,
   Yahoo `b`/`pz`, and Yandex `p=` before shipping paging support, or ship page-1-only for a v1
   and revisit paging as a dedicated follow-up issue?
5. **Mojeek's connectivity.** The ~25-minute TCP-unreachable window this session was reproduced by
   two independent agents on the same network — is that residential-IP/ASN-specific noise that
   might not reproduce elsewhere? It doesn't change the drop verdict (ALTCHA still gates the one
   response that did arrive), but flagging in case it's relevant to future recon on a different
   network.

---

## Fixtures

`fixtures/` holds trimmed raw captures for every keep/maybe engine plus the notable drop
counter-examples, for #66's hermetic tests. Every fixture has a `<!-- ... -->` (HTML) or
`_fixture_note` (JSON) header stating: exact request, capture timestamp, HTTP status, and what it
demonstrates. All cookie/session-token values are redacted (`<redacted>` / `<redacted-...>`); no
`.env`, no live credentials, no residential IP address anywhere in this doc or its fixtures.

| Path | What it is |
|---|---|
| `fixtures/brave/Q1-success.html` | 4 real result rows, direct URLs. |
| `fixtures/brave/Q8-429-blocked.html` + `-headers.txt` | The rate-limit wall; no body marker, status-only detection. |
| `fixtures/bing/Q1-success-mktUS.html` + `-headers.txt` | Corrected-locale success, wrapped URLs. |
| `fixtures/yahoo/Q1-success.html` | 4 real result rows, wrapped URLs. |
| `fixtures/yahoo/YBV-cookie-chain-evidence.txt` | Redacted hop-by-hop cookie-chain trace. |
| `fixtures/yandex/Q1-success-corrected-endpoint.html` | 4 real result rows, correct 6-param shape. |
| `fixtures/yandex/Q1-WRONG-ENDPOINT-empty-200.html` | The empty-but-200 trap — a regression fixture. |
| `fixtures/wikipedia/Q1-success.json` + `Q3-empty-200.json` | Real hit + the documented zero-recall case. |
| `fixtures/marginalia/old-api-Q1-success.json` + `new-api-Q1-429-ratelimited.json` | Working deprecated path vs. dead current path. |
| `fixtures/mwmbl/Q1-success.json` | Real response shape; illustrates the incidental-match relevance problem. |
| `fixtures/ddg-baseline/Q1-cohesion-reference.html` | The DDG Q1 sample every Jaccard number above was computed against. |
| `fixtures/dropped-for-reference/*` | Startpage Anubis, Mojeek ALTCHA, Qwant DataDome, Google `/wml` 403, ChatNoir near-zero/stale-corpus — evidence for each drop verdict, not intended for #66 to build against. |
