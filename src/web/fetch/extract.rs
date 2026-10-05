//! Main-content selection for the static fetch path (#108).
//!
//! The baseline conversion (`fetcher::html_to_markdown` over the whole
//! page minus `SKIP_TAGS`) keeps docs sidebars, tables of contents and site
//! chrome. [`extract`] picks one content subtree with `scraper`: the single
//! `main`, else the single `[role=main]`, else the single `article`, and
//! only when that subtree holds at least [`MIN_TEXT_SHARE`] of the page's
//! text. [`keep_extracted`] is the fail-closed gate: the extraction replaces
//! the baseline only when it loses nothing the agent could need, so any
//! doubt keeps today's output.

use scraper::{ElementRef, Html, Node, Selector};

use super::clean::{fence_open, is_fence_close};

/// Share of the page's text (non-whitespace chars outside `skip_tags`) a
/// candidate subtree must hold to be selected. Measured on the 40 pages in
/// `tests/fixtures/pages/` (#108): every selected subtree held 0.80 to 1.00
/// of the page text (lowest: a Super User thread whose sidebar carries
/// "Related"/"Hot network questions" lists); 0.6 leaves headroom below that.
pub(crate) const MIN_TEXT_SHARE: f64 = 0.6;

/// Smallest extracted/baseline markdown length ratio the gate accepts.
/// Measured on the same set: kept extractions are 0.69 to 1.00 of the
/// baseline (lowest: the same Super User thread); 0.6 leaves headroom while
/// still refusing a subtree that dropped a large part of the page.
pub(crate) const MIN_LENGTH_FRACTION: f64 = 0.6;

/// Markers of a thread or Q&A page: per-answer/per-comment containers
/// (Stack Exchange's `answer`/`comment` classes, schema.org Q&A microdata).
const THREAD_MARKERS: &str = ".answer, .comment, [itemprop=suggestedAnswer], \
     [itemprop=acceptedAnswer], [itemprop=comment], [itemtype$=\"/Answer\"], \
     [itemtype$=\"/Comment\"]";

/// One selected subtree: its outer HTML, its share of the page text, and
/// whether thread/Q&A markers exist outside it (answers it would drop).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Extraction {
    pub(crate) html: String,
    pub(crate) text_share: f64,
    pub(crate) thread_outside: bool,
}

/// The main-content subtree of `html`, or `None` when no candidate is
/// unique or none holds [`MIN_TEXT_SHARE`] of the page text.
pub(crate) fn extract(html: &str, skip_tags: &[&str]) -> Option<Extraction> {
    let document = Html::parse_document(html);
    let page_text = text_chars(document.root_element(), skip_tags);
    if page_text == 0 {
        return None;
    }
    let chosen = ["main", "[role=main]", "article"]
        .iter()
        .find_map(|css| single(&document, css))?;
    let text_share = text_chars(chosen, skip_tags) as f64 / page_text as f64;
    if text_share < MIN_TEXT_SHARE {
        return None;
    }
    let markers = Selector::parse(THREAD_MARKERS).expect("static thread selector");
    let thread_outside = document
        .select(&markers)
        .any(|marker| !marker.ancestors().any(|node| node.id() == chosen.id()));
    Some(Extraction {
        html: chosen.html(),
        text_share,
        thread_outside,
    })
}

/// The only element matching `css`; `None` for zero or several.
fn single<'a>(document: &'a Html, css: &str) -> Option<ElementRef<'a>> {
    let selector = Selector::parse(css).expect("static landmark selector");
    let mut matches = document.select(&selector);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Non-whitespace chars of text under `element`, skipping `skip_tags`
/// subtrees (the same tags the markdown conversion drops).
fn text_chars(element: ElementRef<'_>, skip_tags: &[&str]) -> usize {
    let mut total = 0;
    let mut stack = vec![*element];
    while let Some(node) = stack.pop() {
        match node.value() {
            Node::Text(text) => total += text.chars().filter(|c| !c.is_whitespace()).count(),
            Node::Element(el) if skip_tags.contains(&el.name()) => {}
            _ => stack.extend(node.children()),
        }
    }
    total
}

/// Fail-closed gate (#108): keep `extracted` (its markdown) over `baseline`
/// only when all hold: the extractor succeeded; it is at least
/// `min_markdown_chars` and [`MIN_LENGTH_FRACTION`] of the baseline long;
/// every fenced code block and every table row of the baseline is still
/// present; and no thread/Q&A answer sits outside the subtree.
pub(crate) fn keep_extracted(
    baseline: &str,
    extracted: Option<(&Extraction, &str)>,
    min_markdown_chars: usize,
) -> bool {
    let Some((extraction, markdown)) = extracted else {
        return false;
    };
    if extraction.thread_outside {
        return false;
    }
    let kept = markdown.chars().count();
    let base = baseline.chars().count();
    if kept < min_markdown_chars || (kept as f64) < MIN_LENGTH_FRACTION * base as f64 {
        return false;
    }
    if !fenced_blocks(baseline)
        .iter()
        .all(|block| markdown.contains(block))
    {
        return false;
    }
    let rows: std::collections::HashSet<&str> = table_rows(markdown).collect();
    table_rows(baseline).all(|row| rows.contains(row))
}

/// Every fenced code block of `markdown`, opening and closing fence lines
/// included, using the same fence rules as `clean_markdown`. An unclosed
/// fence runs to the end.
pub(crate) fn fenced_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<((char, usize), Vec<&str>)> = None;
    for line in markdown.split('\n') {
        let trimmed = line.trim_start();
        match open.as_mut() {
            Some(((ch, len), lines)) => {
                lines.push(line);
                if is_fence_close(trimmed, *ch, *len) {
                    blocks.push(lines.join("\n"));
                    open = None;
                }
            }
            None => {
                if let Some(fence) = fence_open(trimmed) {
                    open = Some((fence, vec![line]));
                }
            }
        }
    }
    if let Some((_, lines)) = open {
        blocks.push(lines.join("\n"));
    }
    blocks
}

/// Trimmed table rows of `markdown`: `|`-led lines with at least one more
/// `|`. A lone `|` is a breadcrumb or link-list separator, not a row (the
/// Python docs header renders one).
fn table_rows(markdown: &str) -> impl Iterator<Item = &str> {
    markdown
        .split('\n')
        .map(str::trim)
        .filter(|line| line.starts_with('|') && line[1..].contains('|'))
}

#[cfg(test)]
mod tests;
