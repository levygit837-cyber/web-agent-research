//! Fetch-only search types: providers, recency, hits, merge, fan-out I/O.
//!
//! The `search` tool fans out agent-loop Queries over the enabled engines
//! (DuckDuckGo, Brave, Yahoo, Bing, and opt-in Startpage; all fetch-only
//! HTML, no credentials), dedups hits by URL key and returns merged results
//! ranked by Reciprocal Rank Fusion. Provider text inside hits is an ungrounded
//! candidate: only fetched page content with URL + time becomes Evidence.
//!
//! Ported from oh-my-pi (MIT, can1357/oh-my-pi@83c9df0); see
//! `THIRD-PARTY-NOTICES.md`.

use serde::{Deserialize, Serialize};

/// Per-engine default result count (Omp `DEFAULT_NUM_RESULTS`).
pub const DEFAULT_NUM_RESULTS: usize = 10;
/// Per-engine max result count (Omp `MAX_NUM_RESULTS` via `clampNumResults`).
pub const MAX_NUM_RESULTS: usize = 20;
/// Fan-out default merged count (Omp Public Web default 15).
pub const DEFAULT_TOP_K: usize = 15;
/// Fan-out max merged count (Omp Public Web max 30).
pub const MAX_TOP_K: usize = 30;
/// Max Queries per fan-out call; the agent loop re-plans instead of widening.
pub const MAX_QUERIES: usize = 8;
/// Max length of one Query string (provider form + URL length sanity).
pub const MAX_QUERY_LEN: usize = 500;
/// Per-leg transport ceiling in seconds (Omp 60 s default, max 300).
pub const LEG_TIMEOUT_SECS: u64 = 60;
/// Fan-out soft deadline in seconds: merge early with >=1 success (Omp 5 s).
pub const SOFT_DEADLINE_SECS: u64 = 5;
/// Fan-out hard cap in seconds (Omp 30 s).
pub const HARD_DEADLINE_SECS: u64 = 30;
/// DDG locale default (Omp `localeToKl` default; full KL table is a follow-up).
pub const DDG_KL_DEFAULT: &str = "us-en";
/// Bing `mkt` default (#66: unconditional -- Bing silently geo/language-
/// localizes on client IP with zero explicit signal otherwise, confirmed
/// live in the #65 evaluation: 0/10 relevant results from a Brazilian IP
/// with no `mkt`, 5/5 after adding it). `SEARCH_BING_MARKET` overrides;
/// `setlang` is always derived from this value's language subtag.
pub const BING_MARKET_DEFAULT: &str = "en-US";

/// One search engine the fan-out can query. The set is a closed enum, not a
/// seam (ADR-0006: concrete by default, `trait` only on real variation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchProvider {
    DuckDuckGo,
    Startpage,
    Brave,
    Yahoo,
    Bing,
}

impl SearchProvider {
    /// Every ported provider is credential-free: unconditionally `true`
    /// (Omp `DuckDuckGoProvider::isAvailable` / `StartpageProvider::isAvailable`).
    pub fn is_available(self) -> bool {
        true
    }

    /// Engine priority for deterministic merge iteration (#66: from the
    /// #65 evaluation's measured relevance/breadth data, `docs/research/
    /// search-engines.md` "Verdicts and priority order"). Startpage leads
    /// (opt-in only, but ranked highest per the pre-existing Omp
    /// `PUBLIC_ENGINE_IDS` order); Brave next (P@5 0.80, real breadth over
    /// DDG: 14 unique URLs, Jaccard 0.167); then Yahoo (P@5 0.76, but the
    /// highest DDG overlap of any kept engine, Jaccard 0.70 -- reliable,
    /// low incremental breadth); then DuckDuckGo (P@5 0.76 baseline, the
    /// long-standing default); Bing last (P@5 1.00 with `mkt` but only a
    /// single live sample this session, fastest transport but the
    /// heaviest wrapper to unwrap). Lower value iterates first.
    pub fn priority(self) -> usize {
        match self {
            SearchProvider::Startpage => 0,
            SearchProvider::Brave => 1,
            SearchProvider::Yahoo => 2,
            SearchProvider::DuckDuckGo => 3,
            SearchProvider::Bing => 4,
        }
    }

    /// Whether this engine's request can apply `recency` (#81). Only
    /// windows verified against the live engine count; an engine that
    /// cannot apply the window is skipped by the fan-out when `recency` is
    /// set, never run unfiltered.
    ///
    /// - DuckDuckGo `df` and Startpage `with_date`: `d/w/m/y` (Omp).
    /// - Yahoo `btf`: `d/w/m`, no year window. Verified live 2026-10-02:
    ///   `docs/research/search-engines.md` "Recency windows (#81)".
    /// - Brave `tf`: none. SearXNG documents `pd/pw/pm/py`, but Brave
    ///   answered the one verification request with HTTP 429 (walled from
    ///   this IP), so it stays unverified rather than guessed.
    /// - Bing: none (no recency filter reachable without JS).
    pub fn applies_recency(self, recency: Recency) -> bool {
        match self {
            SearchProvider::DuckDuckGo | SearchProvider::Startpage => true,
            SearchProvider::Yahoo => recency != Recency::Year,
            SearchProvider::Brave | SearchProvider::Bing => false,
        }
    }

    /// Stable lowercase id used in logs, `SEARCH_ENGINES`/`SEARCH_PACE_*`
    /// tokens, the cache key, and the all-failed message.
    pub fn id(self) -> &'static str {
        match self {
            SearchProvider::DuckDuckGo => "duckduckgo",
            SearchProvider::Startpage => "startpage",
            SearchProvider::Brave => "brave",
            SearchProvider::Yahoo => "yahoo",
            SearchProvider::Bing => "bing",
        }
    }
}

/// Freshness window. Form value is `d/w/m/y` for DDG `df`, Startpage
/// `with_date` and Yahoo `btf` (Omp `RECENCY_TO_DDG_DF` /
/// `RECENCY_TO_STARTPAGE_WITH_DATE`: `day->d, week->w, month->m, year->y`);
/// which engines accept which window is [`SearchProvider::applies_recency`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    /// Provider form value for this window (identical for DDG `df`,
    /// Startpage `with_date` and Yahoo `btf`).
    pub fn param(self) -> &'static str {
        match self {
            Recency::Day => "d",
            Recency::Week => "w",
            Recency::Month => "m",
            Recency::Year => "y",
        }
    }

    /// The word the model passes in the `search` tool's `recency` argument.
    pub fn name(self) -> &'static str {
        match self {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        }
    }
}

/// One raw provider hit. Not Evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    /// Link text, trimmed, HTML entities decoded, non-empty.
    pub title: String,
    /// Absolute http(s) URL as returned (not canonicalised).
    pub url: String,
    /// Provider-supplied description; may be empty; NEVER Evidence.
    pub snippet: String,
    pub provider: SearchProvider,
    /// The exact Query string that produced this hit.
    pub query: String,
    /// 0-based position inside its provider/Query leg response
    /// (DDG: global order across continuation pages).
    pub rank: usize,
    /// Best-effort raw date text; only DDG fills it (Omp `extractPublishedDate`).
    pub published_date: Option<String>,
}

/// One deduped entry after `merge_sources`. This is what the handler fetches.
/// Port of Omp `MergedSource` (`source` copy + `engines` + `bestRank` +
/// insertion order), extended with the Query dimension for multi-query
/// fan-out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergedResult {
    /// Title of the best-ranked row in the group (best rank adopts title+URL).
    pub title: String,
    /// Group key (`dedup_key`).
    pub canonical_url: String,
    /// Raw URL of the best-ranked row (fetch + display).
    pub display_url: String,
    /// Longest non-empty provider text (may be empty).
    pub snippet: String,
    /// Sorted, deduped; `len` = Omp `engines`.
    pub providers: Vec<SearchProvider>,
    /// Sorted, deduped; every Query that surfaced this key.
    pub queries: Vec<String>,
    /// Omp `bestRank`: lowest rank seen in the group.
    pub best_rank: usize,
    /// Raw SearchResult rows folded into this group.
    pub hit_count: usize,
    /// First-win (Omp `??=`).
    pub published_date: Option<String>,
}

/// Validated input for one fan-out call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchInput {
    /// 1..=MAX_QUERIES raw strings, each trimmed len 1..=MAX_QUERY_LEN.
    pub queries: Vec<String>,
    /// 1..=MAX_TOP_K merged results to return (default DEFAULT_TOP_K).
    pub top_k: usize,
    /// Mapped to `df` / `with_date`; None sends neither.
    pub recency: Option<Recency>,
}
impl SearchInput {
    /// Validate raw caller input: trim, drop empties, enforce
    /// `1..=MAX_QUERIES` and per-query length, fill `top_k` default.
    /// No HTTP is performed. Violations return `EmptyQuery` / `TooManyQueries`.
    pub fn validate(
        queries: Vec<String>,
        top_k: Option<usize>,
        recency: Option<Recency>,
    ) -> Result<Self, SearchProviderError> {
        let kept: Vec<String> = queries
            .into_iter()
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty())
            .collect();
        if kept.is_empty() {
            return Err(SearchProviderError::EmptyQuery);
        }
        if kept.len() > MAX_QUERIES {
            return Err(SearchProviderError::TooManyQueries {
                got: kept.len(),
                max: MAX_QUERIES,
            });
        }
        for q in &kept {
            // JSON Schema `maxLength` counts Unicode characters, so the
            // validator counts `chars()` too: a 500-char non-ASCII query is
            // legal even though it exceeds 500 bytes. Variant kept per SPEC.
            if q.chars().count() > MAX_QUERY_LEN {
                return Err(SearchProviderError::TooManyQueries {
                    got: q.chars().count(),
                    max: MAX_QUERY_LEN,
                });
            }
        }
        let top_k = top_k.unwrap_or(DEFAULT_TOP_K).clamp(1, MAX_TOP_K);
        Ok(SearchInput {
            queries: kept,
            top_k,
            recency,
        })
    }
}

/// Output of one fan-out call. Partial success is the norm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchOutput {
    /// Merged results ranked by Reciprocal Rank Fusion over (provider, query)
    /// legs, then query coverage, len <= top_k.
    pub results: Vec<MergedResult>,
    /// One entry per failed provider x Query leg, in leg order (deterministic).
    pub errors: Vec<SearchProviderError>,
    pub stats: SearchStats,
    /// Model-facing one-liner when the call ran no leg on purpose (#81:
    /// `recency` set and no enabled engine applies that window). `None`
    /// on every call that dispatched at least one leg.
    pub note: Option<String>,
    /// One short line per engine that ran a leg in this call, in priority
    /// order (#104): `"<engine>: <n> rows"`, `"<engine>: no results"`, or
    /// that engine's distinct leg error(s) (`"bing: suspended (challenge),
    /// 3412s left"`, `"brave: unrecognized markup"`). Shown to the model
    /// when a call returns no Hits, so an empty answer is never mistaken
    /// for "nothing exists" when the engines were walled or drifted.
    pub engine_status: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchStats {
    /// len(input.queries).
    pub queries: usize,
    /// queries * 2 providers attempted.
    pub legs: usize,
    /// SearchResult rows before merge.
    pub raw_hits: usize,
    /// Groups before top_k truncation.
    pub merged: usize,
}

/// Typed failure for one provider x Query leg, plus whole-call failure.
/// Port of Omp `SearchProviderError(provider, message, status)` with its
/// 429 / 504 / 503 numeric mapping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchProviderError {
    /// Input validation: no usable query. HTTP status 400.
    EmptyQuery,
    /// Input validation: too many queries (or over-long query). HTTP 400.
    TooManyQueries { got: usize, max: usize },
    /// Bot wall -> 429.
    Challenge {
        provider: SearchProvider,
        detail: String,
    },
    /// Leg deadline -> 504.
    Timeout {
        provider: SearchProvider,
        query: String,
    },
    /// Network error or non-2xx without challenge markers -> 503. `status`
    /// is the real HTTP status when a response was received (`None` for a
    /// transport-level failure: DNS, connect, TLS, or body-read error).
    /// `retry_after_secs` is the parsed `Retry-After` header (seconds form
    /// only), when the response carried one (#62: wins over the default
    /// suspension duration for 429).
    Upstream {
        provider: SearchProvider,
        detail: String,
        status: Option<u16>,
        retry_after_secs: Option<u64>,
    },
    /// Engine skipped: still under an active suspension recorded from a
    /// prior Challenge/403/429/5xx/transport failure, or from a real
    /// `Retry-After`, on this or an earlier run (#62). No HTTP request was
    /// attempted for this leg. `remaining_secs` is the time left on the
    /// suspension at the moment it was checked.
    Suspended {
        provider: SearchProvider,
        remaining_secs: u64,
        reason: String,
    },
    /// Engine skipped by a local scheduling decision, not a bot-wall
    /// signal: the cap on Queries dispatched to one engine within a call
    /// (`SEARCH_MAX_QUERIES_PER_ENGINE_PER_CALL`) or the per-run request
    /// cap (`SEARCH_MAX_REQUESTS_PER_ENGINE`) was already reached for this
    /// engine (#63). No HTTP request was attempted for this leg. Distinct
    /// from `Suspended`/`Challenge` so it never counts toward
    /// `AllFailed.all_challenged` -- a busy run hitting its own cap says
    /// nothing about the engine's health.
    Throttled {
        provider: SearchProvider,
        detail: String,
    },
    /// Every leg failed AND nothing merged -> 503
    /// (Omp "All public engines failed: ...").
    AllFailed {
        failures: String,
        /// Whether every one of those failed legs was itself a bot-wall
        /// signal -- a live `Challenge` or an active `Suspended` skip
        /// whose `reason` is itself `"challenge"` (#53, extended #62) --
        /// never a `Timeout`/`Upstream`/`Throttled`/non-challenge
        /// `Suspended` mix: the signal `research::agent_loop` uses to
        /// surface a typed `SearchBlocked` failure instead of a silent
        /// empty Synthesis.
        all_challenged: bool,
    },
}

impl SearchProviderError {
    /// Stable numeric mapping for logs, handler budgets, and tests.
    pub fn http_status(&self) -> u16 {
        match self {
            SearchProviderError::EmptyQuery => 400,
            SearchProviderError::TooManyQueries { .. } => 400,
            SearchProviderError::Challenge { .. } => 429,
            SearchProviderError::Timeout { .. } => 504,
            SearchProviderError::Upstream { .. } => 503,
            // A skip, not a live response, but the bot-wall condition it
            // reflects is the same class as `Challenge` (#62).
            SearchProviderError::Suspended { .. } => 429,
            // A local scheduling decision, not a bot-wall signal: same
            // numeric class as `Upstream` (#63).
            SearchProviderError::Throttled { .. } => 503,
            SearchProviderError::AllFailed { .. } => 503,
        }
    }

    /// Stable machine string for logs and tests.
    pub fn code(&self) -> &'static str {
        match self {
            SearchProviderError::EmptyQuery => "empty_query",
            SearchProviderError::TooManyQueries { .. } => "too_many_queries",
            SearchProviderError::Challenge { .. } => "challenge",
            SearchProviderError::Timeout { .. } => "timeout",
            SearchProviderError::Upstream { .. } => "upstream",
            SearchProviderError::Suspended { .. } => "suspended",
            SearchProviderError::Throttled { .. } => "throttled",
            SearchProviderError::AllFailed { .. } => "all_failed",
        }
    }

    /// Owning provider, if the error belongs to one leg.
    pub fn provider(&self) -> Option<SearchProvider> {
        match self {
            SearchProviderError::EmptyQuery => None,
            SearchProviderError::TooManyQueries { .. } => None,
            SearchProviderError::Challenge { provider, .. } => Some(*provider),
            SearchProviderError::Timeout { provider, .. } => Some(*provider),
            SearchProviderError::Upstream { provider, .. } => Some(*provider),
            SearchProviderError::Suspended { provider, .. } => Some(*provider),
            SearchProviderError::Throttled { provider, .. } => Some(*provider),
            SearchProviderError::AllFailed { .. } => None,
        }
    }
}
