//! Answer parsing: plain-text FINAL replies become typed Synthesis.
//!
//! Loop behavior lives here (next to its caller); the output types are
//! catalog data in `research::synthesis`. When a reply carries an
//! `<answer>` block, only that block is parsed, so the model's remarks about
//! its own progress never reach the caller (#72). The only hard gate is
//! non-emptiness: markdown pedantry never fails a good answer.

use crate::research::synthesis::{Citation, Synthesis, SynthesisSize, ThemeSection};
use crate::web::search::dedup_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerError {
    EmptyAnswer,
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyAnswer => write!(f, "empty answer block"),
        }
    }
}

impl std::error::Error for AnswerError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLink {
    title: Option<String>,
    url: String,
}

/// Scan one text span for inline `[title](url)` links, in document order.
/// Other schemes and empty targets are skipped; reference-style links
/// (`[t][r]`) are never parsed by design.
fn scan_links(text: &str) -> Vec<ParsedLink> {
    let mut links = Vec::new();
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let open = match text[cursor..].find('[') {
            Some(offset) => cursor + offset,
            None => break,
        };
        let close_label = match text[open..].find(']') {
            Some(offset) => open + offset,
            None => break,
        };
        let after_label = close_label + 1;
        if after_label >= bytes.len() || bytes[after_label] != b'(' {
            cursor = close_label + 1;
            continue;
        }
        let after_paren = after_label + 1;
        let Some(close_paren_offset) = text[after_paren..].find(')') else {
            break;
        };
        let close_paren = after_paren + close_paren_offset;
        let title_raw = &text[open + 1..close_label];
        let target_raw = text[after_paren..close_paren].trim();
        if target_raw.is_empty()
            || target_raw.contains(char::is_whitespace)
            || !(target_raw.starts_with("http://") || target_raw.starts_with("https://"))
        {
            cursor = close_paren + 1;
            continue;
        }
        let title = title_raw.trim();
        links.push(ParsedLink {
            title: if title.is_empty() {
                None
            } else {
                Some(title.to_owned())
            },
            url: target_raw.to_owned(),
        });
        cursor = close_paren + 1;
    }
    links
}

fn dedupe_links(links: Vec<ParsedLink>) -> Vec<Citation> {
    let mut seen = std::collections::HashSet::new();
    let mut citations = Vec::new();
    for link in links {
        if seen.insert(link.url.clone()) {
            citations.push(Citation {
                url: link.url,
                title: link.title,
            });
        }
    }
    citations
}

fn citations_in(text: &str, global: &[Citation]) -> Vec<Citation> {
    let local_urls: Vec<String> = scan_links(text).into_iter().map(|link| link.url).collect();
    let mut seen = std::collections::HashSet::new();
    let mut section = Vec::new();
    for url in local_urls {
        if !seen.insert(url.clone()) {
            continue;
        }
        if let Some(citation) = global.iter().find(|citation| citation.url == url) {
            section.push(citation.clone());
        }
    }
    section
}

fn bullet_text(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("- ") {
        let text = rest.trim();
        return (!text.is_empty()).then(|| text.to_owned());
    }
    if let Some(rest) = line.strip_prefix("* ") {
        let text = rest.trim();
        return (!text.is_empty()).then(|| text.to_owned());
    }
    let bytes = line.as_bytes();
    let mut digits = 0;
    while digits < bytes.len() && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits == 0 || digits + 1 >= bytes.len() {
        return None;
    }
    if bytes[digits] != b'.' || bytes[digits + 1] != b' ' {
        return None;
    }
    let text = line[digits + 2..].trim();
    (!text.is_empty()).then(|| text.to_owned())
}
/// Opening tag of the final-answer block the system prompt asks for (#72).
pub(crate) const ANSWER_OPEN: &str = "<answer>";
/// Closing tag of the final-answer block.
pub(crate) const ANSWER_CLOSE: &str = "</answer>";

/// The part of a FINAL reply that is the answer: the text between the last
/// `</answer>` and the nearest `<answer>` before it; the text after that
/// `<answer>` when the reply was cut before its closing tag; the text before
/// a stray `</answer>`; otherwise the whole reply. Anchoring on the last
/// closing tag keeps a tag the model merely mentions in its narration from
/// being taken for the block.
fn answer_block(body: &str) -> &str {
    let end = body.rfind(ANSWER_CLOSE).unwrap_or(body.len());
    match body[..end].rfind(ANSWER_OPEN) {
        Some(open) => &body[open + ANSWER_OPEN.len()..end],
        None => &body[..end],
    }
}

pub(crate) fn parse_answer(body: &str, size: SynthesisSize) -> Result<Synthesis, AnswerError> {
    let body = answer_block(body);
    if body.trim().is_empty() {
        return Err(AnswerError::EmptyAnswer);
    }
    let citations = dedupe_links(scan_links(body));

    let mut summary_zone: Vec<&str> = Vec::new();
    let mut themes: Vec<(String, Vec<&str>)> = Vec::new();
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if let Some(title) = line.strip_prefix("## ") {
            let title = title.trim();
            if title.is_empty() {
                continue;
            }
            themes.push((title.to_owned(), Vec::new()));
            continue;
        }
        if themes.is_empty() {
            if !line.is_empty() {
                summary_zone.push(line);
            }
        } else if !line.is_empty() {
            let current = themes.len() - 1;
            themes[current].1.push(line);
        }
    }

    let summary = summary_zone.join(" ");
    let summary = if summary.trim().is_empty() {
        body.split_whitespace()
            .take(24)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        summary
    };

    let mut sections = Vec::new();
    if themes.is_empty() {
        sections.push(ThemeSection {
            title: "Findings".to_owned(),
            points: vec![summary.clone()],
            citations: citations.clone(),
        });
    } else {
        for (title, lines) in themes {
            let joined = lines.join("\n");
            let mut points: Vec<String> =
                lines.iter().filter_map(|line| bullet_text(line)).collect();
            if points.is_empty() {
                let text = lines.join(" ");
                if !text.trim().is_empty() {
                    points.push(text);
                }
            }
            sections.push(ThemeSection {
                title,
                points,
                citations: citations_in(&joined, &citations),
            });
        }
        if sections.iter().all(|section| section.points.is_empty()) {
            sections[0].points.push(summary.clone());
        }
    }

    Ok(Synthesis {
        size,
        summary,
        themes: sections,
        citations,
    })
}

/// Drop every citation whose URL was not fetched as Evidence in this
/// Research, from the whole-answer set and from each theme. `fetched`
/// holds `dedup_key`s, so `www.`/trailing-slash/redirect variants of a
/// fetched page still count. Models link sub-pages they only saw inside
/// fetched markdown; the Harness contract is that every citation was read.
pub(crate) fn retain_fetched_citations(
    synthesis: &mut Synthesis,
    fetched: &std::collections::HashSet<String>,
) {
    let keep = |citation: &Citation| fetched.contains(&dedup_key(&citation.url));
    synthesis.citations.retain(keep);
    for theme in &mut synthesis.themes {
        theme.citations.retain(keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINKED: &str = "# Overview\n\nThe [Obscura engine](https://example.com/obscura) is fast.\n\n## Search\n\n- Use [search docs](https://example.com/search) first.\n- Then [fetch docs](https://example.com/fetch) next.\n\n## Limits\n\nSee the [Obscura engine](https://example.com/obscura) again.";

    #[test]
    fn headings_bullets_and_links_split() {
        let synthesis = parse_answer(LINKED, SynthesisSize::Medium).expect("linked answer parses");
        assert_eq!(synthesis.size, SynthesisSize::Medium);
        assert!(!synthesis.summary.is_empty());
        assert!(synthesis.summary.contains("Obscura engine"));
        assert_eq!(synthesis.themes.len(), 2);
        assert_eq!(synthesis.themes[0].title, "Search");
        assert_eq!(synthesis.themes[0].points.len(), 2);
        assert_eq!(synthesis.themes[0].citations.len(), 2);
        assert_eq!(synthesis.themes[1].title, "Limits");
        assert_eq!(synthesis.themes[1].points.len(), 1);
    }

    #[test]
    fn citations_dedupe_by_exact_url_in_doc_order() {
        let synthesis = parse_answer(LINKED, SynthesisSize::Large).expect("linked answer parses");
        let urls: Vec<&str> = synthesis
            .citations
            .iter()
            .map(|citation| citation.url.as_str())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/obscura",
                "https://example.com/search",
                "https://example.com/fetch"
            ]
        );
        assert_eq!(
            synthesis.citations[0].title.as_deref(),
            Some("Obscura engine")
        );
    }

    #[test]
    fn heading_free_answer_yields_findings_theme() {
        let synthesis = parse_answer(
            "Plain answer with a [link](https://example.com/a).",
            SynthesisSize::Small,
        )
        .expect("heading-free answer parses");
        assert_eq!(synthesis.themes.len(), 1);
        assert_eq!(synthesis.themes[0].title, "Findings");
        assert_eq!(synthesis.themes[0].points.len(), 1);
        assert_eq!(synthesis.citations.len(), 1);
    }

    #[test]
    fn link_free_answer_still_parses() {
        let synthesis = parse_answer("## Solo\n\nJust prose, no links.", SynthesisSize::Small)
            .expect("link-free answer parses");
        assert!(!synthesis.summary.is_empty());
        assert_eq!(synthesis.themes.len(), 1);
        assert!(synthesis.citations.is_empty());
        assert_eq!(synthesis.themes[0].points.len(), 1);
    }

    #[test]
    fn empty_body_is_the_only_error() {
        for raw in ["", "   ", "\n\t \n"] {
            let err = parse_answer(raw, SynthesisSize::Medium).expect_err("empty body must fail");
            assert_eq!(err.to_string(), "empty answer block");
            assert_eq!(err, AnswerError::EmptyAnswer);
        }
    }

    #[test]
    fn size_echoes_requested() {
        for size in [
            SynthesisSize::Small,
            SynthesisSize::Medium,
            SynthesisSize::Large,
            SynthesisSize::Deep,
        ] {
            let synthesis = parse_answer("An answer.", size).expect("non-empty parses");
            assert_eq!(synthesis.size, size);
        }
    }

    #[test]
    fn deep_headings_demote_to_body_text() {
        let synthesis = parse_answer(
            "## Theme\n\n### Detail\n\nBody text here.",
            SynthesisSize::Medium,
        )
        .expect("demoted headings parse");
        assert_eq!(synthesis.themes.len(), 1);
        assert_eq!(synthesis.themes[0].points.len(), 1);
        assert!(synthesis.themes[0].points[0].contains("Detail"));
    }

    #[test]
    fn non_http_schemes_and_reference_links_skipped() {
        let synthesis = parse_answer(
            "See [mail](mailto:a@b.c), [file](ftp://x/y), [empty](), and [ref][r].\n\n[r]: https://example.com/r\n\nBut keep [ok](https://example.com/ok).",
            SynthesisSize::Small,
        )
        .expect("mixed-link answer parses");
        let urls: Vec<&str> = synthesis
            .citations
            .iter()
            .map(|citation| citation.url.as_str())
            .collect();
        assert_eq!(urls, vec!["https://example.com/ok"]);
    }

    #[test]
    fn empty_link_title_is_none() {
        let synthesis = parse_answer("See [](https://example.com/bare).", SynthesisSize::Small)
            .expect("bare link parses");
        assert_eq!(synthesis.citations.len(), 1);
        assert_eq!(synthesis.citations[0].title, None);
    }

    #[test]
    fn numbered_bullets_become_points() {
        let synthesis = parse_answer("## Steps\n\n1. First\n2. Second", SynthesisSize::Medium)
            .expect("numbered list parses");
        assert_eq!(synthesis.themes[0].points, vec!["First", "Second"]);
    }

    /// Models narrate before their final answer ("I have enough information
    /// to provide a comprehensive answer.", measured in 4 of 7 runs in #72);
    /// only the `<answer>` block may reach the caller's Synthesis.
    #[test]
    fn remarks_outside_the_answer_block_never_reach_the_synthesis() {
        let reply = "I have enough information to provide a comprehensive answer.\n\n<answer>\nTokio is an asynchronous runtime for Rust ([Tokio](https://tokio.rs/)).\n\n## Features\n\n- Async I/O and timers ([tokio docs](https://docs.rs/tokio/latest/tokio/)).\n</answer>\n\nLet me know if you need more.";
        let synthesis = parse_answer(reply, SynthesisSize::Small).expect("tagged answer parses");
        assert_eq!(
            synthesis.summary,
            "Tokio is an asynchronous runtime for Rust ([Tokio](https://tokio.rs/))."
        );
        assert_eq!(synthesis.themes.len(), 1);
        assert_eq!(
            synthesis.themes[0].points,
            vec!["Async I/O and timers ([tokio docs](https://docs.rs/tokio/latest/tokio/))."]
        );
        let urls: Vec<&str> = synthesis
            .citations
            .iter()
            .map(|citation| citation.url.as_str())
            .collect();
        assert_eq!(
            urls,
            vec!["https://tokio.rs/", "https://docs.rs/tokio/latest/tokio/"]
        );
    }

    /// A reply cut at the output-token limit loses its closing tag: the text
    /// after the opening tag is still the answer, without the tag itself.
    #[test]
    fn unclosed_answer_block_runs_to_the_end() {
        let synthesis = parse_answer(
            "Checking the pages.\n<answer>\nTokio is a runtime.\n\n## Use\n\n- Spawn tasks.",
            SynthesisSize::Small,
        )
        .expect("unclosed block parses");
        assert_eq!(synthesis.summary, "Tokio is a runtime.");
        assert_eq!(synthesis.themes[0].points, vec!["Spawn tasks."]);
    }

    /// An explicitly empty answer block is an empty answer, which the loop
    /// repairs, never a reason to publish the narration around it.
    #[test]
    fn empty_answer_block_is_an_empty_answer() {
        let err = parse_answer(
            "I have enough information.\n<answer>\n</answer>",
            SynthesisSize::Small,
        )
        .expect_err("empty block must not parse");
        assert_eq!(err, AnswerError::EmptyAnswer);
    }
}
