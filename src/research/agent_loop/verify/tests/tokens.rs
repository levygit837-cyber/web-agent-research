//! Token check (#106): numbers with units, versions, error codes and
//! backticked identifiers must occur on the cited page.

use super::super::{claim_tokens, normalize_text, CheckedPage, Token};
use crate::research::synthesis::Support;

const QUOTE: &str = "Rust release";

fn support(page: &str, claim: &str) -> Support {
    let quote = normalize_text(QUOTE);
    CheckedPage::new(page).support(Some(&quote), &claim_tokens(claim))
}

#[test]
fn version_and_number_cases() {
    let page = "Rust release 1.75.0 is out. It needs 8,192 tokens and runs in 30 s. Fixes E0502.";
    let cases: &[(&str, Support)] = &[
        ("Rust 1.75 is out", Support::Exact),
        ("Rust 1.76 is out", Support::Partial),
        ("Rust v1.75.0 is out", Support::Exact),
        ("Rust 1,75 is out", Support::Exact),
        ("Rust 1.75.1 is out", Support::Partial),
        ("It needs 8192 tokens", Support::Exact),
        ("It needs 4096 tokens", Support::Partial),
        ("It runs in 30 s", Support::Exact),
        ("It runs in 31 s", Support::Partial),
        ("The fix is for E0502", Support::Exact),
        ("The fix is for E0499", Support::Partial),
        ("A claim with no checkable token", Support::Exact),
    ];
    for (claim, expected) in cases {
        assert_eq!(support(page, claim), *expected, "claim: {claim}");
    }
}

/// A page that says `1.75` does not support a claim of `v1.75.0`'s patch
/// digit either way round only when they differ: `1.75` on the page and
/// `1.75.0` in the claim is not a match (the page never said `.0`).
#[test]
fn a_longer_claim_version_needs_the_page_to_say_it() {
    assert_eq!(
        support("Rust release 1.75 notes.", "Rust v1.75.0"),
        Support::Partial
    );
    assert_eq!(
        support("Rust release 1.75 notes.", "Rust 1.75"),
        Support::Exact
    );
}

#[test]
fn identifiers_match_whole_or_by_last_segment() {
    let page =
        "Rust release notes: call `timeout` on the builder; see ClientBuilder::connect_timeout.";
    let cases: &[(&str, Support)] = &[
        ("Use `timeout`", Support::Exact),
        ("Use `reqwest::ClientBuilder::timeout()`", Support::Exact),
        ("Use `ClientBuilder::connect_timeout`", Support::Exact),
        ("Use `read_timeout`", Support::Partial),
        ("Use `tokio::time::sleep`", Support::Partial),
    ];
    for (claim, expected) in cases {
        assert_eq!(support(page, claim), *expected, "claim: {claim}");
    }
}

/// Link URLs and titles are labels, not claims: their digits and paths are
/// never checked.
#[test]
fn links_are_not_claims() {
    let tokens = claim_tokens(
        "Use the builder ([Rust 1.99 docs](https://docs.rs/reqwest/0.12.9/reqwest/)).",
    );
    assert!(tokens.is_empty(), "{tokens:?}");
}

#[test]
fn tokens_are_extracted_once_each() {
    let tokens = claim_tokens("Rust 1.75 and 1.75 again fix E0502 in `foo::bar` taking 30 ms");
    assert_eq!(
        tokens,
        vec![
            Token::Identifier("foo::bar".to_owned()),
            Token::Number(vec!["1".to_owned(), "75".to_owned()]),
            Token::Number(vec!["30".to_owned()]),
            Token::Code("E0502".to_owned()),
        ]
    );
}

/// The quote decides first: missing tokens never lift a bullet whose quote
/// is absent above `none`.
#[test]
fn an_absent_quote_stays_none_whatever_the_tokens() {
    let page = CheckedPage::new("Rust 1.75 notes.");
    let quote = normalize_text("not on this page");
    assert_eq!(
        page.support(Some(&quote), &claim_tokens("Rust 1.75")),
        Support::None
    );
}
