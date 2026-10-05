//! Quote check (#106): normalization on both sides, substring of the page,
//! and the end-to-end grading of a parsed Synthesis.

use std::sync::Arc;

use super::super::{normalize_text, verify_synthesis, CheckedPage};
use crate::research::agent_loop::answer::parse_answer;
use crate::research::agent_loop::types::ToolEvidence;
use crate::research::synthesis::{Support, SynthesisSize, Verification};
use crate::web::fetch::Evidence;

const PAGE: &str = "# Release notes\n\nThe **`Client`** now honors\nthe configured timeout for every request — including redirects.\nSee [the guide](https://example.com/guide) for “connection pooling” details.\n";

fn support(page: &str, quote: Option<&str>) -> Support {
    let quote = quote.map(normalize_text);
    CheckedPage::new(page).support(quote.as_deref(), &[])
}

#[test]
fn quote_cases() {
    let nbsp = "the configured\u{00A0}timeout for every request";
    let cases: &[(&str, Option<&str>, Support)] = &[
        (
            "exact",
            Some("honors the configured timeout"),
            Support::Exact,
        ),
        (
            "reflowed across a line break",
            Some("now honors the configured timeout for every request"),
            Support::Exact,
        ),
        (
            "markdown-wrapped on the page",
            Some("The Client now honors"),
            Support::Exact,
        ),
        (
            "markdown link text on the page",
            Some("See the guide for"),
            Support::Exact,
        ),
        ("NBSP in the quote", Some(nbsp), Support::Exact),
        (
            "curly quotes on the page, straight in the quote",
            Some("for \"connection pooling\" details"),
            Support::Exact,
        ),
        (
            "dash variants",
            Some("every request - including redirects"),
            Support::Exact,
        ),
        (
            "zero-width characters in the quote",
            Some("the con\u{200B}figured time\u{FEFF}out"),
            Support::Exact,
        ),
        (
            "letter case",
            Some("THE CONFIGURED TIMEOUT"),
            Support::Exact,
        ),
        (
            "absent from the page",
            Some("the client ignores the timeout"),
            Support::None,
        ),
        ("empty quote", Some(""), Support::None),
        ("no quote", None, Support::None),
    ];
    for (name, quote, expected) in cases {
        assert_eq!(support(PAGE, *quote), *expected, "case: {name}");
    }
}

fn evidence(url: &str, markdown: &str) -> ToolEvidence {
    ToolEvidence {
        turn: 1,
        id: "call_1_1".to_owned(),
        tool: "fetch".to_owned(),
        excerpt: String::new(),
        url: Some(url.to_owned()),
        fetch_path: None,
        page: Some(Arc::new(Evidence::new(url.to_owned(), markdown.to_owned()))),
    }
}

/// The run's pages grade every bullet; unsupported citations are flagged,
/// never dropped, and a bullet that cites no fetched page is unchecked.
#[test]
fn synthesis_is_graded_and_nothing_is_dropped() {
    let answer = "Summary ([R](https://r.example/notes)).\n\n## T\n\n- The client honors the timeout ([R](https://r.example/notes)) (quote: \"now honors the configured timeout\").\n- It retries forever ([S](https://s.example/)) (quote: \"retries forever\").\n- It was released in 2.4.1 ([R](https://r.example/notes)) (quote: \"Release notes\").\n- An unfetched page ([U](https://u.example/)) (quote: \"anything\").\n- No link at all.";
    let mut synthesis = parse_answer(answer, SynthesisSize::Medium).expect("parses");
    let evidence = [
        evidence("https://r.example/notes", PAGE),
        evidence("https://s.example/", "This page says nothing relevant."),
    ];
    verify_synthesis(&mut synthesis, &evidence);
    let points = &synthesis.themes[0].points;
    let grades: Vec<Support> = points.iter().map(|point| point.support).collect();
    assert_eq!(
        grades,
        vec![
            Support::Exact,
            Support::None,
            Support::Partial,
            Support::Unchecked,
            Support::Unchecked,
        ]
    );
    assert_eq!(
        synthesis.verification,
        Verification {
            bullets: 5,
            cited: 3,
            exact: 1,
            partial: 1,
            none: 1,
        }
    );
    let by_url = |url: &str| {
        synthesis
            .citations
            .iter()
            .find(|citation| citation.url == url)
            .unwrap_or_else(|| panic!("{url} must stay cited"))
    };
    let r = by_url("https://r.example/notes");
    assert_eq!(r.support, Support::Exact, "best over the bullets citing it");
    assert_eq!(
        r.quote.as_deref(),
        Some("now honors the configured timeout")
    );
    let s = by_url("https://s.example/");
    assert_eq!(s.support, Support::None);
    assert_eq!(s.quote.as_deref(), Some("retries forever"));
    assert_eq!(by_url("https://u.example/").support, Support::Unchecked);
    let theme_s = synthesis.themes[0]
        .citations
        .iter()
        .find(|citation| citation.url == "https://s.example/")
        .expect("theme keeps the flagged citation");
    assert_eq!(theme_s.support, Support::None);
}

/// A page fetched under one URL and cited under a `www.`/trailing-slash
/// variant is still the same page.
#[test]
fn cited_url_variants_find_the_page() {
    let mut synthesis = parse_answer(
        "S.\n\n## T\n\n- Timeout ([R](https://www.r.example/notes/)) (quote: \"honors the configured timeout\").",
        SynthesisSize::Small,
    )
    .expect("parses");
    verify_synthesis(&mut synthesis, &[evidence("https://r.example/notes", PAGE)]);
    assert_eq!(synthesis.themes[0].points[0].support, Support::Exact);
    assert_eq!(synthesis.verification.exact, 1);
}
