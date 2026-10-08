//! Query-term coverage demotion for the merged Hit order (#127).
//!
//! RRF counts agreement between engines, not whether a Hit is about the
//! question: on the 2026-10-07 run, a Hit that only matched the topic word
//! (a vendor home page, a download page, the game that shares a language's
//! name) filled 3.1 of the first 10 Hits. A Hit whose title, URL and snippet
//! carry few of a query's terms is scaled down; one that carries them keeps
//! its RRF score. No Hit is dropped, so a sparse snippet only costs rank.

use std::collections::HashSet;

use crate::web::search::types::MergedResult;

/// Query terms a Hit must carry for full credit. A longer query needs only
/// this many: a four-word question is satisfied by a page that states three.
const TERMS_FOR_FULL_CREDIT: usize = 3;

/// Score multiplier of a Hit that carries none of any query's terms. The
/// floor keeps such a Hit ranked by RRF behind the covered ones instead of
/// removing it.
const COVERAGE_FLOOR: f64 = 0.2;

/// Words that say nothing about the topic, in English and Portuguese (the
/// two languages of the golden set). Matching them would credit every Hit.
const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "by",
    "can",
    "do",
    "does",
    "for",
    "from",
    "how",
    "in",
    "is",
    "it",
    "its",
    "my",
    "of",
    "on",
    "or",
    "the",
    "to",
    "vs",
    "versus",
    "what",
    "when",
    "which",
    "why",
    "with",
    "without",
    "you",
    "your",
    "this",
    "that",
    "these",
    "those",
    "use",
    "using",
    "used",
    "should",
    "would",
    "could",
    "into",
    "than",
    "then",
    "there",
    "their",
    "they",
    "them",
    "como",
    "os",
    "um",
    "uma",
    "uns",
    "umas",
    "de",
    "do",
    "da",
    "dos",
    "das",
    "em",
    "no",
    "na",
    "nos",
    "nas",
    "para",
    "por",
    "com",
    "sem",
    "que",
    "qual",
    "quais",
    "quando",
    "onde",
    "ou",
    "ao",
    "aos",
    "funciona",
    "diferenca",
    "quais",
    "sao",
    "quanto",
];

/// Lower-cased alphanumeric terms of `text`, one character or more than one
/// (single characters match too much to mean anything).
fn terms(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|term| term.chars().count() >= 2)
        .map(str::to_lowercase)
}

/// The significant terms of each input query, built once per merge.
pub(super) struct QueryTerms(Vec<HashSet<String>>);

impl QueryTerms {
    pub(super) fn new(queries: &[String]) -> Self {
        QueryTerms(
            queries
                .iter()
                .map(|query| {
                    terms(query)
                        .filter(|term| !STOP_WORDS.contains(&term.as_str()))
                        .collect::<HashSet<_>>()
                })
                .filter(|set| !set.is_empty())
                .collect(),
        )
    }

    /// Score multiplier in `[COVERAGE_FLOOR, 1.0]`: the best query's share of
    /// its terms found in the Hit's title, URL and snippet, capped at
    /// [`TERMS_FOR_FULL_CREDIT`] terms. `1.0` when there is no query to
    /// compare against (ad-hoc fixtures that bypass the fan-out input).
    pub(super) fn factor(&self, hit: &MergedResult) -> f64 {
        if self.0.is_empty() {
            return 1.0;
        }
        let present: HashSet<String> = terms(&hit.title)
            .chain(terms(&hit.display_url))
            .chain(terms(&hit.snippet))
            .collect();
        let best = self
            .0
            .iter()
            .map(|query| {
                let found = query.iter().filter(|term| present.contains(*term)).count();
                (found as f64 / query.len().min(TERMS_FOR_FULL_CREDIT) as f64).min(1.0)
            })
            .fold(0.0, f64::max);
        COVERAGE_FLOOR + (1.0 - COVERAGE_FLOOR) * best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::search::types::SearchProvider;

    fn merged(title: &str, url: &str, snippet: &str) -> MergedResult {
        MergedResult {
            title: title.to_owned(),
            canonical_url: url.to_owned(),
            display_url: url.to_owned(),
            snippet: snippet.to_owned(),
            providers: vec![SearchProvider::DuckDuckGo],
            queries: vec![],
            best_rank: 0,
            hit_count: 1,
            published_date: None,
        }
    }

    fn queries(items: &[&str]) -> QueryTerms {
        QueryTerms::new(&items.iter().map(|q| (*q).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn covered_hit_keeps_full_score() {
        let q = queries(&["PostgreSQL json vs jsonb differences"]);
        let hit = merged(
            "JSON Types",
            "https://www.postgresql.org/docs/current/datatype-json.html",
            "PostgreSQL offers two types for storing JSON data: json and jsonb.",
        );
        assert_eq!(q.factor(&hit), 1.0);
    }

    #[test]
    fn navigational_hit_is_demoted_to_the_floor() {
        let q = queries(&["Rust Mutex vs RwLock differences"]);
        let hit = merged(
            "Survive on Steam",
            "https://store.steampowered.com/app/252490",
            "Survive.",
        );
        assert_eq!(q.factor(&hit), COVERAGE_FLOOR);
    }

    #[test]
    fn partial_coverage_scales_between_floor_and_one() {
        let q = queries(&["rust mutex rwlock poisoning"]);
        let hit = merged(
            "Rust Mutex",
            "https://doc.rust-lang.org/std/sync/struct.Mutex.html",
            "",
        );
        let expected = COVERAGE_FLOOR + (1.0 - COVERAGE_FLOOR) * (2.0 / 3.0);
        assert!((q.factor(&hit) - expected).abs() < 1e-12);
    }

    #[test]
    fn best_query_decides() {
        let q = queries(&["unrelated words here", "docs.rs anyhow"]);
        let hit = merged("anyhow - Rust", "https://docs.rs/anyhow", "");
        assert_eq!(q.factor(&hit), 1.0);
    }

    #[test]
    fn stop_words_and_single_letters_earn_no_credit() {
        let q = queries(&["how to use the a"]);
        // No significant term: the query is ignored, so the factor is neutral.
        assert_eq!(q.factor(&merged("Anything", "https://x.test/", "")), 1.0);
    }

    #[test]
    fn no_queries_is_neutral() {
        assert_eq!(
            QueryTerms::new(&[]).factor(&merged("t", "https://x.test/", "")),
            1.0
        );
    }

    #[test]
    fn merge_ranks_off_topic_hit_below_covered_hit_even_at_a_better_rank() {
        use crate::web::search::dedup::merge_sources_in_order;
        use crate::web::search::types::SearchResult;

        let query = "PostgreSQL json vs jsonb differences".to_owned();
        let row = |rank: usize, title: &str, url: &str, snippet: &str| SearchResult {
            title: title.to_owned(),
            url: url.to_owned(),
            snippet: snippet.to_owned(),
            provider: SearchProvider::DuckDuckGo,
            query: query.clone(),
            rank,
            published_date: None,
        };
        let merged = merge_sources_in_order(
            vec![
                row(
                    0,
                    "PostgreSQL: Downloads",
                    "https://www.postgresql.org/download/",
                    "Packages.",
                ),
                row(
                    1,
                    "JSON Types",
                    "https://www.postgresql.org/docs/current/datatype-json.html",
                    "The json and jsonb data types differ in storage.",
                ),
            ],
            std::slice::from_ref(&query),
        );
        assert_eq!(merged[0].title, "JSON Types");
        assert_eq!(merged[1].title, "PostgreSQL: Downloads");
    }
}
