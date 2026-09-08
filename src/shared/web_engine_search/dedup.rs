//! Dedup + consensus merge for the fetch-only web search engine.
//!
//! Raw provider hits fold into deduped groups keyed by URL identity;
//! each group becomes one MergedResult for the agent-loop Queries.

use std::collections::HashMap;

use crate::shared::types::search::{MergedResult, SearchResult};

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

/// Fold raw hits into deduped groups. Port of Omp `mergeSources` + consensus
/// sort, with the Query dimension added for multi-query fan-out: iterate rows
/// grouped by leg in engine priority order (Startpage legs first in Query
/// order, then DDG legs in Query order), never arrival order. Per group the
/// best rank adopts title + display URL, longest snippet wins, published date
/// is first-win. Sort: provider count desc, best rank asc, display URL asc.
/// `queries` is the validated fan-out input order: legs key by
/// `(priority, query_index)` so duplicate query strings never collapse.
pub fn merge_sources(results: Vec<SearchResult>) -> Vec<MergedResult> {
    merge_sources_in_order(results, &[])
}

/// Order-aware merge core: `queries[i]` is the input string of leg index `i`.
/// Rows whose `query` is absent from `queries` (ad-hoc unit fixtures) fall
/// back to first-seen index order, preserving the old deterministic output.
pub(crate) fn merge_sources_in_order(
    results: Vec<SearchResult>,
    queries: &[String],
) -> Vec<MergedResult> {
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
    let mut groups: HashMap<String, MergedResult> = HashMap::new();
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
                        MergedResult {
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
                    );
                }
                Some(group) => {
                    if !group.providers.contains(&row.provider) {
                        group.providers.push(row.provider);
                        group.providers.sort_by_key(|p| p.priority());
                    }
                    if !group.queries.contains(&row.query) {
                        group.queries.push(row.query.clone());
                        group.queries.sort();
                    }
                    if row.rank < group.best_rank {
                        group.best_rank = row.rank;
                        group.title.clone_from(&row.title);
                        group.display_url.clone_from(&row.url);
                    }
                    if !row.snippet.is_empty() && row.snippet.len() > group.snippet.len() {
                        group.snippet.clone_from(&row.snippet);
                    }
                    if group.published_date.is_none() {
                        group.published_date.clone_from(&row.published_date);
                    }
                    group.hit_count += 1;
                }
            }
        }
    }
    let mut out: Vec<MergedResult> = groups.into_values().collect();
    out.sort_by(|a, b| {
        b.providers
            .len()
            .cmp(&a.providers.len())
            .then_with(|| a.best_rank.cmp(&b.best_rank))
            .then_with(|| a.display_url.cmp(&b.display_url))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::types::search::SearchProvider;

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
    fn merge_consensus_order() {
        // Two-provider group outranks any single-provider group, even with a
        // worse best rank; best-ranked title/URL adopted; longest snippet
        // wins; first-win date; URL-asc tie-break.
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
        // utm variant group has 2 providers -> first; bare-URL single next;
        // solo last (URL-asc tie-break among singletons by best rank).
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
        // Singletons sort by provider count tie -> best_rank asc -> URL asc.
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
        // input list proves the SPEC §8 contract directly.
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
}
