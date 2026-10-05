//! Page continuation (#80): parts served from memory, parts past the end,
//! and one delivery per page across redirecting URLs.

use super::super::*;
use super::helpers::{delivered, fetch_part, kind, long_page_html, reason, TEST_PART_CHARS};

#[tokio::test]
async fn long_page_comes_in_parts_served_from_memory() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &long_page_html())).await;
    let url = format!("{}/long", pages.base());
    let registry = ToolRegistry::offline();

    let first = fetch_part(&registry, &url, None).await;
    let page = match &first {
        ToolResult::Fetch {
            evidence,
            part: 1,
            parts: 3,
            first_delivery: true,
            ..
        } => Arc::clone(evidence),
        other => panic!("expected part 1 of 3 on first delivery, got {other:?}"),
    };
    let chars = page.markdown.chars().count();
    assert!(
        chars > 2 * TEST_PART_CHARS && chars <= 3 * TEST_PART_CHARS,
        "the fixture must be a 3-part page, got {chars} chars"
    );
    assert_eq!(pages.hits("/"), 1);
    let rendered = first.render("n");
    assert!(rendered.starts_with(&format!("Source: {}\n", page.source_url)));
    assert!(
        rendered.contains("part=2"),
        "a part that is not the last must say how to get the next: {}",
        &rendered[rendered.len().saturating_sub(120)..]
    );

    let second = fetch_part(&registry, &url, Some(2)).await;
    let third = fetch_part(&registry, &url, Some(3)).await;
    for (result, expected_part) in [(&second, 2), (&third, 3)] {
        match result {
            ToolResult::Fetch {
                part,
                parts: 3,
                first_delivery: false,
                ..
            } => assert_eq!(*part, expected_part),
            other => panic!("expected a continuation of part {expected_part}, got {other:?}"),
        }
    }
    assert_eq!(
        pages.hits("/"),
        1,
        "continuation parts must be served from memory"
    );
    assert!(
        !third.render("n").contains("call fetch"),
        "the last part has no next"
    );

    let joined = [delivered(&first), delivered(&second), delivered(&third)].concat();
    assert_eq!(joined, page.markdown, "the parts must tile the page");
}

/// A `part` past the end fails with the page's real part count. The
/// model has seen nothing, so the page stays undelivered: the next
/// valid call is its first delivery (the one the Session persists) and
/// costs no second request.
#[tokio::test]
async fn part_past_the_end_fails_and_leaves_the_page_undelivered() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &long_page_html())).await;
    let url = format!("{}/long", pages.base());
    let registry = ToolRegistry::offline();

    let past = fetch_part(&registry, &url, Some(9)).await;
    assert_eq!(kind(&past), FailureKind::Dispatch);
    assert!(past.first_delivery_page().is_none());
    let failure = reason(past);
    assert!(
        failure.contains("3"),
        "the failure must name the part count: {failure}"
    );
    assert_eq!(pages.hits("/"), 1, "the failed call still fetched the page");

    // No part was delivered, so a plain repeat delivers part 1 now
    // instead of pointing at content the model never saw.
    let first = fetch_part(&registry, &url, None).await;
    assert!(
        matches!(
            first,
            ToolResult::Fetch {
                part: 1,
                parts: 3,
                first_delivery: true,
                ..
            }
        ),
        "got {first:?}"
    );
    assert!(first.first_delivery_page().is_some());

    let four = fetch_part(&registry, &url, Some(4)).await;
    assert_eq!(kind(&four), FailureKind::Dispatch);
    assert!(reason(four).contains("3"));
    let later = fetch_part(&registry, &url, Some(2)).await;
    assert!(later.first_delivery_page().is_none(), "already delivered");
    assert_eq!(pages.hits("/"), 1);
}

/// After a part reached the model, a repeat without `part` is the #43
/// pointer and tells the model how many parts the page has.
#[tokio::test]
async fn repeat_without_part_points_at_the_parts() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &long_page_html())).await;
    let url = format!("{}/long", pages.base());
    let registry = ToolRegistry::offline();

    fetch_part(&registry, &url, None).await;
    let repeat = fetch_part(&registry, &url, None).await;
    assert!(
        matches!(repeat, ToolResult::AlreadyFetched { parts: 3, .. }),
        "got {repeat:?}"
    );
    let rendered = repeat.render("n");
    assert!(rendered.contains("ALREADY FETCHED"), "{rendered}");
    assert!(
        rendered.contains('3'),
        "the part count must be stated: {rendered}"
    );
    assert_eq!(pages.hits("/"), 1);
}

/// Two requested URLs that redirect to one final URL are one page: the
/// second call must not become a second first delivery (a second
/// Session row for the same `source_url`), and its parts come from the
/// page already stored.
#[tokio::test]
async fn two_urls_redirecting_to_one_page_deliver_it_once() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|path: &str, _: &str| match path {
        "/final" => StubReply::text(200, &long_page_html()),
        _ => StubReply::redirect(302, "/final"),
    })
    .await;
    let registry = ToolRegistry::offline();

    let via_a = fetch_part(&registry, &format!("{}/a", pages.base()), None).await;
    let via_b = fetch_part(&registry, &format!("{}/b", pages.base()), None).await;
    let (
        ToolResult::Fetch {
            evidence: page_a, ..
        },
        ToolResult::Fetch {
            evidence: page_b, ..
        },
    ) = (&via_a, &via_b)
    else {
        panic!("expected two Fetch results, got {via_a:?} and {via_b:?}");
    };
    assert_eq!(page_a.source_url, page_b.source_url);
    assert!(Arc::ptr_eq(page_a, page_b), "one stored page per final URL");
    assert!(via_a.first_delivery_page().is_some());
    assert!(
        via_b.first_delivery_page().is_none(),
        "the same page must not be persisted twice"
    );
}

#[tokio::test]
async fn first_fetch_can_ask_for_a_later_part_and_a_bad_part_is_rejected_offline() {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &long_page_html())).await;
    let url = format!("{}/long", pages.base());
    let registry = ToolRegistry::offline();

    let zero = fetch_part(&registry, &url, Some(0)).await;
    assert_eq!(kind(&zero), FailureKind::Dispatch);
    assert_eq!(
        pages.hits("/"),
        0,
        "a bad part must fail before any request"
    );

    let second = fetch_part(&registry, &url, Some(2)).await;
    assert!(
        matches!(
            second,
            ToolResult::Fetch {
                part: 2,
                parts: 3,
                first_delivery: true,
                ..
            }
        ),
        "got {second:?}"
    );
    assert!(
        second.first_delivery_page().is_some(),
        "a network fetch is the page's first delivery whatever part it asked for"
    );
}
