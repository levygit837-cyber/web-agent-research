//! Replay-through (#129): a `fetch` miss goes live once, is appended to
//! `pages.jsonl`, and the next run serves it from the tape.

use std::path::PathBuf;

use super::super::*;
use super::helpers::{call, kind, page_html, reason, TEST_PART_CHARS};
use crate::research::agent_loop::tape::{Fixture, Tape, REPLAY_MISS};
use crate::web::search::test_support::{StubReply, StubServer};

/// A fixture dir holding empty `search.jsonl` and `pages.jsonl`.
fn empty_fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("war-replay-through-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    for name in ["search.jsonl", "pages.jsonl"] {
        std::fs::write(dir.join(name), "").expect("empty fixture file");
    }
    dir
}

/// A fresh registry for one run over `dir`, as `run_research` builds it.
fn run_over(dir: &std::path::Path) -> ToolRegistry {
    let tape = Tape::replay_through(dir).expect("tape opens");
    ToolRegistry::offline().with_tape(Some(tape))
}

async fn fetch(registry: &ToolRegistry, url: &str) -> ToolResult {
    registry
        .execute(
            &call("fetch", &serde_json::json!({ "url": url }).to_string()),
            TEST_PART_CHARS,
        )
        .await
}

fn page_lines(dir: &std::path::Path) -> usize {
    std::fs::read_to_string(dir.join("pages.jsonl"))
        .expect("pages.jsonl")
        .lines()
        .count()
}

#[tokio::test]
async fn a_miss_is_fetched_once_appended_and_served_from_the_tape_next_run() {
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
    let url = format!("{}/page", pages.base());
    let dir = empty_fixture("miss");

    let first = fetch(&run_over(&dir), &url).await;
    let ToolResult::Fetch { evidence, .. } = &first else {
        panic!("expected Fetch, got {first:?}");
    };
    let live_markdown = evidence.markdown.clone();
    assert_eq!(pages.hits("/"), 1, "the miss goes live");
    assert_eq!(page_lines(&dir), 1, "and is appended in the record format");

    let second = fetch(&run_over(&dir), &url).await;
    let ToolResult::Fetch { evidence, .. } = &second else {
        panic!("expected Fetch, got {second:?}");
    };
    assert_eq!(evidence.markdown, live_markdown);
    assert_eq!(pages.hits("/"), 1, "the next run replays, no live fetch");
    assert_eq!(page_lines(&dir), 1, "a replayed page is not appended again");
}

#[tokio::test]
async fn a_live_failure_is_appended_and_replayed_as_the_same_failure() {
    let dir = empty_fixture("failure");
    let url = "http://127.0.0.1:9/gone";

    let first = fetch(&run_over(&dir), url).await;
    assert_eq!(kind(&first), FailureKind::Execution);
    let live_reason = reason(first);
    assert!(live_reason.starts_with("fetch failed"), "{live_reason}");
    assert_eq!(page_lines(&dir), 1);

    let second = fetch(&run_over(&dir), url).await;
    assert_eq!(reason(second), live_reason);
    assert_eq!(page_lines(&dir), 1);
}

#[tokio::test]
async fn plain_replay_still_fails_a_miss_and_appends_nothing() {
    let pages = StubServer::serve(|_: &str, _: &str| StubReply::text(200, &page_html())).await;
    let url = format!("{}/page", pages.base());
    let dir = empty_fixture("plain");
    let tape = Tape::Replay(Fixture::load(&dir).expect("fixture loads"));
    let registry = ToolRegistry::offline().with_tape(Some(tape));

    let result = fetch(&registry, &url).await;
    assert!(reason(result).starts_with(REPLAY_MISS));
    assert_eq!(pages.hits("/"), 0);
    assert_eq!(page_lines(&dir), 0);
}
