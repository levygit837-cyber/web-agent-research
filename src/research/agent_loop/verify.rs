//! Model-free grounding check of cited bullets (#106).
//!
//! Each bullet that links a page fetched this run is checked against the
//! stored cleaned page (`Evidence.markdown`, the whole page, not the part
//! the model read):
//!
//! - Quote check: the bullet's `(quote: "…")` must be a substring of the
//!   page once both sides are normalized ([`normalize_text`]: whitespace,
//!   quote marks, dashes, zero-width characters, markdown emphasis and link
//!   syntax). No quote, or a quote not on the page, is [`Support::None`].
//! - Token check: numbers with a unit, versions and decimals, error codes
//!   (`E0502`) and backticked identifiers in the bullet must occur on the
//!   page; one missing token lowers [`Support::Exact`] to
//!   [`Support::Partial`].
//!
//! The check flags, never drops: citations and bullets stay as written.
//! Pure functions over the run's Evidence; no I/O.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::research::agent_loop::types::ToolEvidence;
use crate::research::synthesis::{Citation, Point, Support, Synthesis, Verification};
use crate::web::search::dedup_key;

/// Check every bullet of `synthesis` against the pages fetched in this run
/// (`evidence` items that carry the whole page) and record the outcome on
/// bullets, citations and the [`Verification`] counters.
pub(crate) fn verify_synthesis(synthesis: &mut Synthesis, evidence: &[ToolEvidence]) {
    let mut pages: HashMap<String, CheckedPage> = HashMap::new();
    for page in evidence.iter().filter_map(|item| item.page.as_deref()) {
        pages
            .entry(dedup_key(&page.source_url))
            .or_insert_with(|| CheckedPage::new(&page.markdown));
    }
    let mut verification = Verification::default();
    // Best support per cited URL, with the quote of the bullet that gave it.
    let mut best: HashMap<String, (Support, Option<String>)> = HashMap::new();
    for theme in &mut synthesis.themes {
        for point in &mut theme.points {
            verification.bullets += 1;
            check_point(point, &pages, &mut best);
            if point.support == Support::Unchecked {
                continue;
            }
            verification.cited += 1;
            match point.support {
                Support::Exact => verification.exact += 1,
                Support::Partial => verification.partial += 1,
                Support::None | Support::Unchecked => verification.none += 1,
            }
        }
    }
    let apply = |citation: &mut Citation| {
        if let Some((support, quote)) = best.get(&citation.url) {
            citation.support = *support;
            citation.quote.clone_from(quote);
        }
    };
    synthesis.citations.iter_mut().for_each(&apply);
    for theme in &mut synthesis.themes {
        theme.citations.iter_mut().for_each(&apply);
    }
    synthesis.verification = verification;
}

/// Grade one bullet against each fetched page it links; its support is the
/// best over those pages. A bullet that links no fetched page stays
/// [`Support::Unchecked`].
fn check_point(
    point: &mut Point,
    pages: &HashMap<String, CheckedPage>,
    best: &mut HashMap<String, (Support, Option<String>)>,
) {
    let quote = point.quote.as_deref().map(normalize_text);
    let tokens = claim_tokens(&point.text);
    for url in &point.links {
        let Some(page) = pages.get(&dedup_key(url)) else {
            continue;
        };
        let support = page.support(quote.as_deref(), &tokens);
        point.support = point.support.max(support);
        let entry = best
            .entry(url.clone())
            .or_insert((Support::Unchecked, None));
        if support > entry.0 {
            *entry = (support, point.quote.clone());
        }
    }
}

/// One stored page, prepared once for every bullet that cites it.
struct CheckedPage {
    /// [`normalize_text`] of the page, for the quote check.
    text: String,
    /// The page with whitespace collapsed and markdown escapes removed, for
    /// identifiers and error codes.
    plain: String,
    /// Digit groups of every number on the page (`1.75.0` → `[1, 75, 0]`).
    numbers: Vec<Vec<String>>,
}

impl CheckedPage {
    fn new(markdown: &str) -> Self {
        Self {
            text: normalize_text(markdown),
            plain: plain_text(markdown),
            numbers: NUMBER_RE
                .find_iter(markdown)
                .map(|found| digit_groups(found.as_str()))
                .collect(),
        }
    }

    fn support(&self, quote: Option<&str>, tokens: &[Token]) -> Support {
        match quote {
            Some(quote) if !quote.is_empty() && self.text.contains(quote) => {}
            _ => return Support::None,
        }
        if tokens.iter().all(|token| self.has(token)) {
            Support::Exact
        } else {
            Support::Partial
        }
    }

    fn has(&self, token: &Token) -> bool {
        match token {
            Token::Number(groups) => self.numbers.iter().any(|page| numbers_match(groups, page)),
            Token::Code(code) => CODE_RE
                .find_iter(&self.plain)
                .any(|found| found.as_str() == code),
            Token::Identifier(ident) => {
                self.plain.contains(ident.as_str())
                    || last_segment(ident).is_some_and(|segment| self.plain.contains(segment))
            }
        }
    }
}

/// A checkable fact in a bullet.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A version, a decimal, or a number with a unit, as digit groups.
    Number(Vec<String>),
    /// An error code such as `E0502`.
    Code(String),
    /// The text of a backticked span.
    Identifier(String),
}

/// The checkable tokens of a bullet's text. Link targets and titles are
/// labels, not claims, so links are removed before scanning.
fn claim_tokens(text: &str) -> Vec<Token> {
    let text = LINK_RE.replace_all(text, "");
    let mut tokens: Vec<Token> = Vec::new();
    let mut push = |token: Token| {
        if !tokens.contains(&token) {
            tokens.push(token);
        }
    };
    for caps in BACKTICK_RE.captures_iter(&text) {
        let ident = caps[1].trim();
        if !ident.is_empty() {
            push(Token::Identifier(ident.replace('\\', "")));
        }
    }
    let prose = BACKTICK_RE.replace_all(&text, " ");
    for caps in VERSION_RE.captures_iter(&prose) {
        push(Token::Number(digit_groups(&caps[1])));
    }
    for caps in UNIT_RE.captures_iter(&prose) {
        push(Token::Number(digit_groups(&caps[1])));
    }
    for found in CODE_RE.find_iter(&prose) {
        push(Token::Code(found.as_str().to_owned()));
    }
    tokens
}

/// `1.75.0` → `["1", "75", "0"]`; `.` and `,` both separate groups, so a
/// decimal comma (`1,75`) matches a decimal point (`1.75`).
fn digit_groups(number: &str) -> Vec<String> {
    number
        .split(['.', ','])
        .filter(|group| !group.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a bullet number matches a page number: the same groups; the same
/// value once thousands separators are dropped (`8,192` vs `8192`); or, for
/// a version, a prefix of the page's version (`1.75` vs `1.75.0`).
fn numbers_match(claim: &[String], page: &[String]) -> bool {
    if claim == page {
        return true;
    }
    if let (Some(claim), Some(page)) = (joined_thousands(claim), joined_thousands(page)) {
        if claim == page {
            return true;
        }
    }
    claim.len() >= 2 && page.starts_with(claim)
}

fn joined_thousands(groups: &[String]) -> Option<String> {
    let thousands = groups.len() > 1 && groups[1..].iter().all(|group| group.len() == 3);
    if groups.len() == 1 || thousands {
        Some(groups.concat())
    } else {
        None
    }
}

/// Last segment of a path-like identifier (`tokio::time::timeout()` →
/// `timeout`), or `None` when the identifier has a single segment.
fn last_segment(ident: &str) -> Option<&str> {
    let trimmed = ident.trim_end_matches("()");
    let segment = trimmed
        .rsplit([':', '.', '/', '#'])
        .next()
        .filter(|segment| !segment.is_empty())?;
    (segment.len() < trimmed.len()).then_some(segment)
}

/// Normalize text for the quote check, on page and quote alike: markdown
/// links and images keep only their text; emphasis, code and escape marks
/// go; curly quotes become straight, dashes become `-`, `…` becomes `...`;
/// zero-width characters and soft hyphens go; any whitespace run (NBSP
/// included) becomes one space; letters are lowercased.
pub(crate) fn normalize_text(text: &str) -> String {
    let unlinked = LINK_RE.replace_all(text, "$1");
    let mut out = String::with_capacity(unlinked.len());
    let mut space = false;
    for c in unlinked.chars() {
        let mapped = match c {
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}' => continue,
            '*' | '_' | '`' | '\\' => continue,
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' | '\u{00AB}'
            | '\u{00BB}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            c if c.is_whitespace() => {
                space = !out.is_empty();
                continue;
            }
            c => c,
        };
        if space {
            out.push(' ');
            space = false;
        }
        if mapped == '\u{2026}' {
            out.push_str("...");
        } else {
            out.extend(mapped.to_lowercase());
        }
    }
    out
}

/// Page text for identifier and code lookups: markdown escapes removed and
/// whitespace runs collapsed, case and punctuation kept.
fn plain_text(markdown: &str) -> String {
    markdown
        .replace('\\', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

static LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"!?\[([^\]]*)\]\([^)\s]*(?:\s+"[^"]*")?\)"#).expect("static link regex")
});

static BACKTICK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([^`]+)`").expect("static backtick regex"));

/// Versions and decimals: two or more digit groups, optional `v`.
static VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bv?(\d+(?:[.,]\d+)+)\b").expect("static version regex"));

/// Numbers followed by a unit (`30 s`, `256 MiB`, `8192 tokens`, `50%`).
static UNIT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(\d+(?:[.,]\d+)*)\s?(?:%|(?:ns|µs|us|ms|s|secs?|seconds?|mins?|minutes?|h|hrs?|hours?|days?|weeks?|months?|years?|[kmgtp]i?b|bytes?|b|bits?|chars?|characters?|tokens?|lines?|px|[kmg]?hz|x)\b)",
    )
    .expect("static unit regex")
});

/// Error codes: 1-4 capital letters then 3-5 digits (`E0502`, `TS2322`).
static CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z]{1,4}\d{3,5}\b").expect("static code regex"));

/// Every number on a page, separators included.
static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(?:[.,]\d+)*").expect("static number regex"));

#[cfg(test)]
mod tests {
    mod quote;
    mod tokens;
}
