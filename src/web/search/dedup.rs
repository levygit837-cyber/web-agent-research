//! Dedup + Reciprocal Rank Fusion merge for the fetch-only web search engine.
//!
//! Raw provider hits fold into deduped groups keyed by URL identity;
//! each group becomes one MergedResult for the agent-loop Queries.
//!
//! Ported from oh-my-pi (MIT, can1357/oh-my-pi@83c9df0); see
//! `THIRD-PARTY-NOTICES.md`.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::web::search::types::{MergedResult, SearchResult};

/// Normalised identity of a result URL used for dedup: lowercase host minus
/// one leading `www.` + path minus one trailing `/` (only when len > 1) +
/// `?query` preserved verbatim + no fragment. Literal port of Omp `dedupKey`.
/// Total: never panics; parse failure returns the trimmed raw input.
pub fn dedup_key(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    let Some(host) = parsed.host_str() else {
        return trimmed.to_string();
    };
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let mut key = String::with_capacity(trimmed.len());
    key.push_str(host);
    let path = parsed.path();
    if path.len() > 1 {
        key.push_str(path.strip_suffix('/').unwrap_or(path));
    } else if path == "/" && parsed.query().is_none() && trimmed.ends_with('/') {
        // Root path: keep bare host ("/a/" vs "/a" rule only applies to len > 1).
    } else if path != "/" {
        key.push_str(path);
    }
    if let Some(query) = parsed.query() {
        key.push('?');
        key.push_str(query);
    }
    key
}

/// Reciprocal Rank Fusion constant `k` in `score = Σ 1 / (k + rank)`. 60 is
/// the value from the original RRF paper (Cormack, Clarke, Buettcher, SIGIR
/// 2009); it flattens the gap between neighbouring ranks so agreement across
/// legs outweighs a single leg's top position.
const RRF_K: f64 = 60.0;

/// Fold raw hits into deduped groups ranked by Reciprocal Rank Fusion (#110).
/// Port of Omp `mergeSources` with the Query dimension added for multi-query
/// fan-out: iterate rows grouped by leg in engine priority order (#66:
/// Startpage, Brave, Yahoo, DuckDuckGo, Bing -- see
/// `SearchProvider::priority`), each in Query order, never arrival order. Per
/// group the best rank adopts title + display URL, longest snippet wins,
/// published date is first-win.
///
/// Sort: RRF score desc (one `1 / (60 + rank)` term per distinct
/// (provider, query) leg that returned the URL, using that leg's best
/// 1-based rank), then `queries.len()` desc, best rank asc, display URL asc.
/// `queries` is the validated fan-out input order: legs key by
/// `(priority, query_index)`.
pub fn merge_sources(results: Vec<SearchResult>) -> Vec<MergedResult> {
    merge_sources_in_order(results, &[])
}

/// Order-aware merge core: `queries[i]` is the input string of leg index `i`.
/// Rows whose `query` is absent from `queries` (ad-hoc unit fixtures) fall
/// back to first-seen index order, preserving the old deterministic output.
/// The RRF score is not part of `MergedResult`; it is logged at debug level.
pub(crate) fn merge_sources_in_order(
    results: Vec<SearchResult>,
    queries: &[String],
) -> Vec<MergedResult> {
    merge_scored(results, queries)
        .into_iter()
        .map(|(group, score)| {
            tracing::debug!(
                url = %group.canonical_url,
                score,
                providers = group.providers.len(),
                queries = group.queries.len(),
                best_rank = group.best_rank,
                "merged search result"
            );
            group
        })
        .collect()
}

/// One dedup group while folding: the result, the best 0-based rank of each
/// (provider, query) leg that returned it, and the leg last folded in (legs
/// are processed one at a time, so a repeat of `last_leg` is the same leg).
struct Group {
    merged: MergedResult,
    leg_ranks: Vec<usize>,
    last_leg: (usize, usize),
}

/// `Σ 1 / (k + rank)` over 0-based `leg_ranks`, converted to the 1-based
/// ranks RRF is defined on. Sorts `leg_ranks` ascending first so the float
/// sum has one canonical order: permuting the input cannot change the bits.
fn rrf_score(leg_ranks: &mut [usize]) -> f64 {
    leg_ranks.sort_unstable();
    leg_ranks
        .iter()
        .map(|&rank| 1.0 / (RRF_K + rank as f64 + 1.0))
        .sum()
}

/// Final order: score desc (`total_cmp`), coverage desc, best rank asc,
/// display URL asc.
fn compare_scored(a: &(MergedResult, f64), b: &(MergedResult, f64)) -> Ordering {
    b.1.total_cmp(&a.1)
        .then_with(|| b.0.queries.len().cmp(&a.0.queries.len()))
        .then_with(|| a.0.best_rank.cmp(&b.0.best_rank))
        .then_with(|| a.0.display_url.cmp(&b.0.display_url))
}

/// Merge core returning each group with its RRF score, already sorted.
fn merge_scored(results: Vec<SearchResult>, queries: &[String]) -> Vec<(MergedResult, f64)> {
    let mut legs: HashMap<(usize, usize), Vec<SearchResult>> = HashMap::new();
    let mut fallback_next: usize = queries.len();
    let mut fallback_index: HashMap<String, usize> = HashMap::new();
    for row in results {
        let query_index = queries
            .iter()
            .position(|q| *q == row.query)
            .unwrap_or_else(|| {
                *fallback_index.entry(row.query.clone()).or_insert_with(|| {
                    let index = fallback_next;
                    fallback_next += 1;
                    index
                })
            });
        legs.entry((row.provider.priority(), query_index))
            .or_default()
            .push(row);
    }
    let mut leg_keys: Vec<(usize, usize)> = legs.keys().cloned().collect();
    leg_keys.sort();
    let mut groups: HashMap<String, Group> = HashMap::new();
    for leg in leg_keys {
        let rows = legs.remove(&leg).unwrap_or_default();
        for row in rows {
            let key = dedup_key(&row.url);
            if key.trim().is_empty() {
                continue;
            }
            match groups.get_mut(&key) {
                None => {
                    groups.insert(
                        key.clone(),
                        Group {
                            merged: MergedResult {
                                title: row.title.clone(),
                                canonical_url: key,
                                display_url: row.url.clone(),
                                snippet: row.snippet.clone(),
                                providers: vec![row.provider],
                                queries: vec![row.query.clone()],
                                best_rank: row.rank,
                                hit_count: 1,
                                published_date: row.published_date.clone(),
                            },
                            leg_ranks: vec![row.rank],
                            last_leg: leg,
                        },
                    );
                }
                Some(group) => {
                    if group.last_leg == leg {
                        if let Some(best) = group.leg_ranks.last_mut() {
                            *best = (*best).min(row.rank);
                        }
                    } else {
                        group.leg_ranks.push(row.rank);
                        group.last_leg = leg;
                    }
                    let merged = &mut group.merged;
                    if !merged.providers.contains(&row.provider) {
                        merged.providers.push(row.provider);
                        merged.providers.sort_by_key(|p| p.priority());
                    }
                    if !merged.queries.contains(&row.query) {
                        merged.queries.push(row.query.clone());
                        merged.queries.sort();
                    }
                    if row.rank < merged.best_rank {
                        merged.best_rank = row.rank;
                        merged.title.clone_from(&row.title);
                        merged.display_url.clone_from(&row.url);
                    }
                    if !row.snippet.is_empty() && row.snippet.len() > merged.snippet.len() {
                        merged.snippet.clone_from(&row.snippet);
                    }
                    if merged.published_date.is_none() {
                        merged.published_date.clone_from(&row.published_date);
                    }
                    merged.hit_count += 1;
                }
            }
        }
    }
    let mut out: Vec<(MergedResult, f64)> = groups
        .into_values()
        .map(|mut group| {
            let score = rrf_score(&mut group.leg_ranks);
            (group.merged, score)
        })
        .collect();
    out.sort_by(compare_scored);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::types::SearchProvider;

    fn hit(
        provider: SearchProvider,
        query: &str,
        rank: usize,
        url: &str,
        title: &str,
        snippet: &str,
    ) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            url: url.to_string(),
            snippet: snippet.to_string(),
            provider,
            query: query.to_string(),
            rank,
            published_date: None,
        }
    }

    #[test]
    fn dedup_key_vectors() {
        // www/case/trailing-slash/fragment collapse ...
        let cases = [
            ("https://example.com/a", "example.com/a"),
            ("https://WWW.EXAMPLE.COM/a", "example.com/a"),
            ("http://www.example.com/a#frag", "example.com/a"),
            ("https://example.com/a/", "example.com/a"),
        ];
        for (raw, want) in cases {
            assert_eq!(dedup_key(raw), want, "raw={raw}");
        }
        // ... but the query string is preserved (Omp `dedupKey`):
        // tracking-param variants do NOT deduplicate.
        assert_ne!(
            dedup_key("https://example.com/a?utm_source=x"),
            dedup_key("https://example.com/a"),
        );
    }

    #[test]
    fn merge_rrf_order_and_best_rank_adoption() {
        // The two-leg group (2/61) outranks the single-leg groups; best-ranked
        // title/URL adopted; longest snippet wins; first-win date; among the
        // single-leg groups the better rank scores higher.
        let rows = vec![
            hit(
                SearchProvider::DuckDuckGo,
                "b",
                0,
                "https://solo.example/x",
                "Solo",
                "s",
            ),
            hit(
                SearchProvider::Startpage,
                "a",
                1,
                "https://example.com/shared",
                "SP title",
                "short",
            ),
            hit(
                SearchProvider::DuckDuckGo,
                "b",
                0,
                "https://example.com/shared?utm_source=x",
                "DDG title",
                "a much longer snippet",
            ),
            hit(
                SearchProvider::Startpage,
                "a",
                0,
                "https://example.com/shared?utm_source=x",
                "SP better",
                "mid",
            ),
        ];
        let merged = merge_sources(rows);
        // utm variant group has 2 legs at rank 1 (2/61) -> first; the solo
        // group (1/61) next; the bare-URL group (1/62) last.
        assert_eq!(merged.len(), 3);
        let top = &merged[0];
        assert_eq!(top.providers.len(), 2);
        assert_eq!(top.best_rank, 0);
        assert_eq!(top.title, "SP better");
        assert_eq!(top.snippet, "a much longer snippet");
        assert_eq!(top.canonical_url, "example.com/shared?utm_source=x");
        assert_eq!(top.display_url, "https://example.com/shared?utm_source=x");
        assert_eq!(top.hit_count, 2);
        assert_eq!(top.queries, vec!["a".to_string(), "b".to_string()],);
        // Single-leg groups order by RRF score, i.e. by rank.
        assert_eq!(merged[1].best_rank, 0);
        assert_eq!(merged[1].canonical_url, "solo.example/x");
        assert_eq!(merged[2].canonical_url, "example.com/shared");
    }

    #[test]
    fn merge_input_order_beats_alphabetical_on_ties() {
        // Two Startpage legs whose query strings sort opposite to input
        // order: shared-URL adoption is strict `<` on rank, so equal ranks
        // keep the input-first leg's title. `merge_sources` falls back to
        // first-seen order for unknown queries; the order-aware core with the
        // input list proves the input-order-wins contract directly.
        let rows = vec![
            hit(
                SearchProvider::Startpage,
                "zebra",
                0,
                "https://example.com/shared",
                "Zebra Title",
                "s",
            ),
            hit(
                SearchProvider::Startpage,
                "apple",
                0,
                "https://example.com/shared",
                "Apple Title",
                "s",
            ),
        ];
        let order = vec!["zebra".to_string(), "apple".to_string()];
        let merged = merge_sources_in_order(rows, &order);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "Zebra Title");
        assert_eq!(
            merged[0].queries,
            vec!["apple".to_string(), "zebra".to_string()]
        );
    }

    #[test]
    fn merge_engine_priority_order_startpage_brave_yahoo_ddg_bing() {
        // #66: five legs for the same query and URL, each providing a
        // distinct title -- the adopted title/URL must come from whichever
        // provider has the lowest `priority()` value among ties (equal
        // rank), proving the concrete order documented on
        // `SearchProvider::priority`: Startpage < Brave < Yahoo <
        // DuckDuckGo < Bing.
        let rows = vec![
            hit(
                SearchProvider::Bing,
                "q",
                0,
                "https://example.com/shared",
                "Bing title",
                "s",
            ),
            hit(
                SearchProvider::DuckDuckGo,
                "q",
                0,
                "https://example.com/shared",
                "DDG title",
                "s",
            ),
            hit(
                SearchProvider::Yahoo,
                "q",
                0,
                "https://example.com/shared",
                "Yahoo title",
                "s",
            ),
            hit(
                SearchProvider::Brave,
                "q",
                0,
                "https://example.com/shared",
                "Brave title",
                "s",
            ),
        ];
        let merged = merge_sources(rows);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].title, "Brave title",
            "equal ranks: the lowest-priority (Startpage-then-Brave) leg processed first wins the tie"
        );
        assert_eq!(merged[0].providers.len(), 4);
        assert_eq!(
            merged[0].providers,
            vec![
                SearchProvider::Brave,
                SearchProvider::Yahoo,
                SearchProvider::DuckDuckGo,
                SearchProvider::Bing,
            ],
            "providers list stays sorted by priority regardless of input order"
        );
    }

    /// RRF fixture row. `rank` is 1-based, the convention RRF is defined on;
    /// the row stores it 0-based like real providers do.
    fn leg_row(provider: SearchProvider, query: &str, rank: usize, url: &str) -> SearchResult {
        hit(provider, query, rank - 1, url, url, "")
    }

    type LegSpec = (SearchProvider, &'static str, usize, &'static str);

    fn rows_from(specs: &[LegSpec]) -> Vec<SearchResult> {
        specs
            .iter()
            .map(|&(provider, query, rank, url)| leg_row(provider, query, rank, url))
            .collect()
    }

    fn scored(url: &str, queries: usize, best_rank: usize, score: f64) -> (MergedResult, f64) {
        let merged = MergedResult {
            title: url.to_string(),
            canonical_url: url.to_string(),
            display_url: url.to_string(),
            snippet: String::new(),
            providers: vec![SearchProvider::Brave],
            queries: (0..queries).map(|i| format!("q{i}")).collect(),
            best_rank,
            hit_count: 1,
            published_date: None,
        };
        (merged, score)
    }

    /// Every permutation of `items`, by recursive prefix selection.
    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..items.len() {
            let mut rest = items.to_vec();
            let head = rest.remove(i);
            for mut tail in permutations(&rest) {
                tail.insert(0, head.clone());
                out.push(tail);
            }
        }
        out
    }

    #[test]
    fn rrf_ordering_table() {
        use SearchProvider::{Brave, DuckDuckGo, Yahoo};
        let one_leg = "https://one-leg.example/x";
        let two_legs = "https://two-legs.example/x";
        let wide = "https://wide.example/x";
        let narrow = "https://narrow.example/x";
        // Sorts after `a_single` by URL, so winning proves coverage beats URL.
        let z_cover = "https://z-coverage.example/x";
        let a_single = "https://a-single-query.example/x";
        let cases: Vec<(&str, Vec<LegSpec>, Vec<&str>)> = vec![
            (
                // 2/65 = 0.0308 > 1/61 = 0.0164: two independent legs agreeing
                // on rank 5 carry more weight than one leg's top slot. The old
                // engine-count order gave the same winner, for a different
                // reason.
                "rank 1 by one leg vs rank 5 by two legs: the two-leg URL wins",
                vec![
                    (Brave, "q", 1, one_leg),
                    (Yahoo, "q", 5, two_legs),
                    (DuckDuckGo, "q", 5, two_legs),
                ],
                vec![two_legs, one_leg],
            ),
            (
                // 3/61 = 0.0492 > 2/65 = 0.0308: query coverage from one
                // engine beats two engines that each saw it once at rank 5.
                // The old engine-count-first order ranked `narrow` first.
                "one engine x 3 queries at rank 1 vs two engines x 1 query at rank 5",
                vec![
                    (Brave, "q1", 1, wide),
                    (Brave, "q2", 1, wide),
                    (Brave, "q3", 1, wide),
                    (Yahoo, "q1", 5, narrow),
                    (DuckDuckGo, "q1", 5, narrow),
                ],
                vec![wide, narrow],
            ),
            (
                // Both score 2/63 (bit-identical terms); coverage 2 vs 1 decides
                // before the URL does.
                "equal score: more distinct queries first",
                vec![
                    (Brave, "q1", 3, z_cover),
                    (Brave, "q2", 3, z_cover),
                    (Yahoo, "q1", 3, a_single),
                    (DuckDuckGo, "q1", 3, a_single),
                ],
                vec![z_cover, a_single],
            ),
        ];
        for (name, specs, want) in cases {
            let merged = merge_sources(rows_from(&specs));
            let got: Vec<&str> = merged.iter().map(|m| m.display_url.as_str()).collect();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn rrf_single_engine_survivor_orders_by_leg_rank_and_coverage() {
        use SearchProvider::Brave;
        // Only Brave answered, two queries. Every group has one provider, so
        // the old order fell to best rank: `top_once` (best rank 1) first.
        // RRF: `twice` = 1/62 + 1/62 = 0.0323 > `top_once` = 1/61 = 0.0164 >
        // `third` = 1/63.
        let specs: Vec<LegSpec> = vec![
            (Brave, "q1", 1, "https://top-once.example/x"),
            (Brave, "q1", 2, "https://twice.example/x"),
            (Brave, "q2", 2, "https://twice.example/x"),
            (Brave, "q1", 3, "https://third.example/x"),
        ];
        let merged = merge_sources(rows_from(&specs));
        let got: Vec<&str> = merged.iter().map(|m| m.display_url.as_str()).collect();
        assert_eq!(
            got,
            vec![
                "https://twice.example/x",
                "https://top-once.example/x",
                "https://third.example/x",
            ]
        );
        assert!(merged.iter().all(|m| m.providers == vec![Brave]));
    }

    #[test]
    fn rrf_score_is_one_based_and_sum_order_independent() {
        assert_eq!(
            rrf_score(&mut [0]),
            1.0 / 61.0,
            "0-based rank 0 is RRF rank 1"
        );
        assert_eq!(rrf_score(&mut [4, 4]), 1.0 / 65.0 + 1.0 / 65.0);
        let baseline = rrf_score(&mut [0, 3, 6, 11, 29]).to_bits();
        for mut ranks in permutations(&[0usize, 3, 6, 11, 29]) {
            assert_eq!(rrf_score(&mut ranks).to_bits(), baseline, "{ranks:?}");
        }
    }

    #[test]
    fn rrf_merge_is_deterministic_under_input_permutation() {
        use SearchProvider::{Brave, DuckDuckGo, Yahoo};
        let specs: Vec<LegSpec> = vec![
            (Brave, "q1", 1, "https://a.example/x"),
            (Yahoo, "q1", 3, "https://a.example/x"),
            (DuckDuckGo, "q2", 7, "https://a.example/x"),
            (Brave, "q1", 2, "https://b.example/x"),
            (Yahoo, "q2", 2, "https://c.example/x"),
        ];
        let queries = vec!["q1".to_string(), "q2".to_string()];
        let baseline = merge_scored(rows_from(&specs), &queries);
        // `b` and `c` tie on score and coverage and best rank: URL decides.
        let order: Vec<&str> = baseline
            .iter()
            .map(|(m, _)| m.display_url.as_str())
            .collect();
        assert_eq!(
            order,
            vec![
                "https://a.example/x",
                "https://b.example/x",
                "https://c.example/x"
            ]
        );
        let perms = permutations(&rows_from(&specs));
        assert_eq!(perms.len(), 120);
        for rows in perms {
            let got = merge_scored(rows, &queries);
            assert_eq!(got.len(), baseline.len());
            for ((g, gs), (b, bs)) in got.iter().zip(&baseline) {
                assert_eq!(g, b);
                assert_eq!(
                    gs.to_bits(),
                    bs.to_bits(),
                    "score bits for {}",
                    g.display_url
                );
            }
        }
    }

    #[test]
    fn rrf_counts_a_leg_once_with_its_best_rank() {
        use SearchProvider::Brave;
        // One leg returning the same URL twice (`www.` / trailing-slash
        // variants) contributes a single term at its best rank (2), not two.
        let rows = vec![
            leg_row(Brave, "q", 5, "https://dup.example/x/"),
            leg_row(Brave, "q", 2, "https://www.dup.example/x"),
        ];
        let merged = merge_scored(rows, &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0.hit_count, 2);
        assert_eq!(merged[0].1, 1.0 / 62.0);
    }

    #[test]
    fn compare_scored_tiebreak_chain() {
        let better = scored("https://z.example/x", 1, 9, 0.5);
        let worse = scored("https://a.example/x", 9, 0, 0.4);
        assert_eq!(
            compare_scored(&better, &worse),
            Ordering::Less,
            "score first"
        );
        let wide = scored("https://z.example/x", 2, 9, 0.5);
        let narrow = scored("https://a.example/x", 1, 0, 0.5);
        assert_eq!(
            compare_scored(&wide, &narrow),
            Ordering::Less,
            "then coverage"
        );
        let early = scored("https://z.example/x", 1, 0, 0.5);
        let late = scored("https://a.example/x", 1, 4, 0.5);
        assert_eq!(
            compare_scored(&early, &late),
            Ordering::Less,
            "then best rank"
        );
        let first = scored("https://a.example/x", 1, 0, 0.5);
        let second = scored("https://b.example/x", 1, 0, 0.5);
        assert_eq!(compare_scored(&first, &second), Ordering::Less, "then URL");
    }
}
