//! What one executed tool call produced: [`ToolResult`] and its rendering
//! into the tool-role observation text, plus the [`FailureKind`] the runner
//! reads to pick `LoopError::InvalidToolCall` vs. `LoopError::ToolFailed`.
//!
//! Pure data and pure rendering: no I/O, no registry state. Dispatch and
//! per-run fetch memory live in `registry.rs`.

use std::ops::Range;
use std::sync::Arc;

use crate::web::fetch::tool::FETCH_TOOL_NAME;
use crate::web::fetch::{Evidence, FetchPath};
use crate::web::search::types::MergedResult;

/// Whether a [`ToolResult::Failed`] came from validating the call itself
/// (bad name/JSON/args, never reaching the executor) or from the executor
/// actually running and failing (search/fetch error, timeout, exhausted
/// stub). The runner uses this to pick `LoopError::InvalidToolCall` vs.
/// `LoopError::ToolFailed` without re-parsing rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Dispatch,
    Execution,
}

/// One executed tool call, rendered into the tool-role transcript.
#[derive(Debug, Clone)]
pub enum ToolResult {
    /// Hits are candidates: rendered for the model, never Evidence.
    Search { hits: Vec<MergedResult> },
    /// The `search` call ran no leg on purpose (#81: `recency` set, no
    /// enabled engine applies it). Success, not Hits and not a failure:
    /// the note tells the model to search again without `recency`.
    SearchNote { note: String },
    /// One part of a fetched page (#80). `evidence` is the whole stored
    /// page; `span` is the byte range of `evidence.markdown` this result
    /// delivers, `part` of `parts` (1-based). `first_delivery` is true only
    /// when this call fetched the page over the network.
    Fetch {
        evidence: Arc<Evidence>,
        span: Range<usize>,
        part: u32,
        parts: u32,
        first_delivery: bool,
    },
    /// A repeat `fetch` of a URL already fetched in this Research (#43)
    /// with no `part`: no network I/O ran. `url` is the requested URL for
    /// this call; `first_url` is the canonical `Evidence.source_url` from
    /// the earlier fetch; `parts` is how many parts the stored page has.
    /// Success, not Evidence: `url()` is `None`.
    AlreadyFetched {
        url: String,
        first_url: String,
        parts: u32,
    },
    /// Every Startpage/DuckDuckGo leg of this `search` call was a bot-wall
    /// Challenge (#53), i.e. `SearchProviderError::AllFailed { all_challenged: true, .. }`:
    /// the request never reached a usable results page. Distinct from the
    /// generic `Failed` so the runner can remember it across the whole run
    /// (`RunReport::search_blocked`) even though it counts the same as an
    /// execution failure for the repair budget. `detail` carries the
    /// per-engine challenge messages (Omp `formatSearchProviderFailures`).
    SearchBlocked { detail: String },
    #[cfg(test)]
    Hang,
    Failed {
        tool: String,
        reason: String,
        kind: FailureKind,
    },
}

impl ToolResult {
    /// Render one call's result into observation text. Uncapped pure
    /// rendering; the runner caps every result except `Fetch`.
    pub fn render(&self) -> String {
        match self {
            Self::SearchNote { note } => note.clone(),
            Self::Search { hits } => {
                if hits.is_empty() {
                    return "No Hits for these queries. Rephrase once with different words; if that still finds nothing, answer from the pages you already fetched and say what is missing.".to_owned();
                }
                let noun = if hits.len() == 1 { "Hit" } else { "Hits" };
                let mut text = format!(
                    "{} {noun}. Candidates only, not Evidence: fetch a URL before stating or citing anything from its snippet.",
                    hits.len()
                );
                for (index, hit) in hits.iter().enumerate() {
                    text.push_str(&format!(
                        "\n{}. {}\n   {}",
                        index + 1,
                        hit.title,
                        hit.display_url
                    ));
                    if !hit.snippet.is_empty() {
                        text.push_str(&format!("\n   {}", hit.snippet));
                    }
                }
                text
            }
            // Source first: the model needs the final URL to cite. The
            // runner never caps a Fetch result; the part is already sized.
            Self::Fetch {
                evidence,
                span,
                part,
                parts,
                ..
            } => {
                let mut text = format!("Source: {}\n", evidence.source_url);
                if *parts > 1 {
                    text.push_str(&format!("Part {part} of {parts}\n"));
                }
                text.push('\n');
                text.push_str(&evidence.markdown[span.clone()]);
                if part < parts {
                    text.push_str(&format!(
                        "\n[part {part} of {parts}; call fetch with the same url and part={} for the rest]",
                        part + 1
                    ));
                }
                text
            }
            Self::AlreadyFetched {
                url,
                first_url,
                parts,
            } => {
                let mut text = format!(
                    "ALREADY FETCHED: {url} was already fetched earlier in this Research (as {first_url}). Use that earlier content, or fetch a different Hit."
                );
                text.push_str(&format!(
                    " The page has {parts} {}; call fetch with the same url and part=k to read part k again (served from memory, no new request).",
                    plural_parts(*parts)
                ));
                text
            }
            Self::SearchBlocked { detail } => format!(
                "FAILED: search blocked: {detail}\nEvery enabled search engine answered with a bot-detection wall, so searching again in this run fails the same way. Answer from pages you already fetched, or fetch URLs named in the research goal."
            ),
            #[cfg(test)]
            Self::Hang => "hanging".to_owned(),
            Self::Failed { tool, reason, kind } => {
                if tool == FETCH_TOOL_NAME && *kind == FailureKind::Execution {
                    format!("FAILED: {reason}\nThis page could not be read, so it is not Evidence: fetch a different Hit instead of retrying this URL.")
                } else {
                    format!("FAILED: {reason}")
                }
            }
        }
    }

    /// Source URL of fetched Evidence. Search Hits and `AlreadyFetched`
    /// carry none: neither is Evidence.
    pub fn url(&self) -> Option<String> {
        match self {
            Self::Fetch { evidence, .. } => Some(evidence.source_url.clone()),
            _ => None,
        }
    }

    /// Which engine produced the fetched Evidence (#31); `None` for every
    /// non-`Fetch` result.
    pub fn fetch_path(&self) -> Option<FetchPath> {
        match self {
            Self::Fetch { evidence, .. } => evidence.fetch_path,
            _ => None,
        }
    }

    /// The whole page, when this result is the first time it reached the
    /// model this run: the one the Session persists (#80). Continuation
    /// parts and repeats return `None`.
    pub fn first_delivery_page(&self) -> Option<Arc<Evidence>> {
        match self {
            Self::Fetch {
                evidence,
                first_delivery: true,
                ..
            } => Some(Arc::clone(evidence)),
            _ => None,
        }
    }

    /// Execution worked (even an empty hit list, or a skipped repeat fetch,
    /// is information, not failure). `SearchBlocked` is not: the bot wall
    /// stopped the call from reaching a usable page.
    pub fn is_success(&self) -> bool {
        !matches!(self, Self::Failed { .. } | Self::SearchBlocked { .. })
    }

    /// `Some` naming which kind of failure this is; `None` for a success.
    /// `SearchBlocked` executed and failed at the provider, same as any
    /// other search execution error.
    pub fn failure_kind(&self) -> Option<FailureKind> {
        match self {
            Self::Failed { kind, .. } => Some(*kind),
            Self::SearchBlocked { .. } => Some(FailureKind::Execution),
            _ => None,
        }
    }
}

pub(super) fn plural_parts(parts: u32) -> &'static str {
    if parts == 1 {
        "part"
    } else {
        "parts"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::agent_loop::test_support::test_hit;

    #[test]
    fn only_fetch_results_carry_evidence_urls() {
        let search = ToolResult::Search {
            hits: vec![test_hit("T", "https://example.com/t")],
        };
        assert_eq!(search.url(), None);
        let fetch = ToolResult::Fetch {
            evidence: Arc::new(Evidence::new(
                "https://example.com/p".to_owned(),
                "Body".to_owned(),
            )),
            span: 0..4,
            part: 1,
            parts: 1,
            first_delivery: true,
        };
        assert_eq!(fetch.url().as_deref(), Some("https://example.com/p"));
    }

    /// The answer parser turns inline `[title](url)` links into citations,
    /// so a Hit rendered in that syntax invites the model to copy it as a
    /// citation for a page it never read (#72): Hits list plain URLs.
    #[test]
    fn hits_never_render_as_citation_links() {
        let rendered = ToolResult::Search {
            hits: vec![test_hit("T", "https://example.com/t")],
        }
        .render();
        assert!(rendered.contains("https://example.com/t"), "{rendered}");
        assert!(!rendered.contains("]("), "{rendered}");
    }

    /// Citations survive only when they match the fetched page's final URL,
    /// so every part must lead with it, and a part that is not the last
    /// must say how to get the next one.
    #[test]
    fn every_part_leads_with_the_source_url_and_points_to_the_next() {
        let page = Arc::new(Evidence::new(
            "https://example.com/final".to_owned(),
            "aaaabbbbcc".to_owned(),
        ));
        let render = |span: Range<usize>, part: u32| {
            ToolResult::Fetch {
                evidence: Arc::clone(&page),
                span,
                part,
                parts: 3,
                first_delivery: true,
            }
            .render()
        };
        let first = render(0..4, 1);
        assert!(first.starts_with("Source: https://example.com/final\n"));
        assert!(first.contains("aaaa") && !first.contains("bbbb"));
        assert!(first.contains("part=2"), "{first}");
        let last = render(8..10, 3);
        assert!(last.starts_with("Source: https://example.com/final\n"));
        assert!(last.contains("cc") && !last.contains("bbbb"));
        assert!(!last.contains("part="), "the last part has no next: {last}");
    }

    #[test]
    fn a_single_part_page_renders_without_part_markers() {
        let page = Arc::new(Evidence::new(
            "https://example.com/p".to_owned(),
            "Body".to_owned(),
        ));
        let rendered = ToolResult::Fetch {
            evidence: page,
            span: 0..4,
            part: 1,
            parts: 1,
            first_delivery: true,
        }
        .render();
        assert!(rendered.starts_with("Source: https://example.com/p\n"));
        assert!(rendered.ends_with("Body"));
        assert!(!rendered.contains("part"), "{rendered}");
        assert!(!rendered.contains("Part"), "{rendered}");
    }
}
