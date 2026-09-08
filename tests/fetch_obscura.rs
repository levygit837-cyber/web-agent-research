//! Hermetic suite for the Obscura fetch integration (issue #14).
//!
//! The engine under test points at `tests/fixtures/obscura-fake.sh`, a shell
//! double dispatching on the positional URL ($2). No real `obscura` binary is
//! needed; the single live check lives in `tests/live_obscura.rs` (ignored).

use web_agent_research::shared::session::Evidence;

#[test]
fn evidence_new_stamps_rfc3339_collection_time() {
    let ev = Evidence::new(
        "https://example.com/page".to_string(),
        "# Title".to_string(),
    );
    assert_eq!(ev.source_url, "https://example.com/page");
    assert_eq!(ev.markdown, "# Title");
    // RFC 3339 UTC, second precision: YYYY-MM-DDTHH:MM:SSZ.
    assert_eq!(
        ev.collected_at.len(),
        20,
        "collected_at = {}",
        ev.collected_at
    );
    assert!(ev.collected_at.ends_with('Z'));
    assert_eq!(&ev.collected_at[10..11], "T");
}

#[test]
fn evidence_round_trips_through_json() {
    let ev = Evidence::new(
        "https://example.com/a".to_string(),
        "body".to_string(),
    );
    let back: Evidence =
        serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(ev, back);
}
