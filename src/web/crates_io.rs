//! crates.io data-access policy shared by the search leg
//! (`web::search::cratesio`) and the crate-page fetch rewrite
//! (`web::fetch::cratesio`), #109.
//!
//! The policy: the sparse index (`index.crates.io`) has no rate limit;
//! the API allows at most 1 request per second per client, with a
//! User-Agent that names the application and a contact
//! ([`super::api_user_agent`]). Every API request in the process passes
//! [`API_GATE`] first, so the search leg and the fetch rewrite together
//! never exceed that rate.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// crates.io API root (`/api/v1`).
pub(crate) const API_BASE: &str = "https://crates.io/api/v1";

/// Sparse-index root.
pub(crate) const INDEX_BASE: &str = "https://index.crates.io";

/// Minimum gap between two API requests from this process: the policy's
/// 1 request/s in production, short under `cfg(test)` so stub-backed
/// tests do not sleep (the gate itself is tested with its own instance).
#[cfg(not(test))]
const API_GAP: Duration = Duration::from_secs(1);
#[cfg(test)]
const API_GAP: Duration = Duration::from_millis(5);

/// The process-wide API gate.
pub(crate) static API_GATE: ApiGate = ApiGate::new(API_GAP);

/// Spaces callers at least `gap` apart, in arrival order. Each caller
/// reserves the next free slot under the lock and sleeps outside it, so a
/// slow caller never holds the lock across an await.
pub(crate) struct ApiGate {
    gap: Duration,
    next_free: Mutex<Option<Instant>>,
}

impl ApiGate {
    pub(crate) const fn new(gap: Duration) -> Self {
        Self {
            gap,
            next_free: Mutex::new(None),
        }
    }

    /// Wait for this caller's slot.
    pub(crate) async fn wait(&self) {
        let slot = {
            let mut next_free = self
                .next_free
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            let slot = next_free.map_or(now, |next| next.max(now));
            *next_free = Some(slot + self.gap);
            slot
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(slot)).await;
    }
}

/// crates.io name rule: 1-64 ASCII alphanumerics, `-` or `_`, starting
/// with a letter.
pub(crate) fn is_crate_name(raw: &str) -> bool {
    let mut chars = raw.chars();
    raw.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Sparse-index file path of a valid crate name (cargo's layout, all
/// lowercase): `1/<n>` and `2/<n>` for 1- and 2-char names, `3/<c>/<n>`
/// for 3-char names, else `<first two>/<next two>/<n>`.
pub(crate) fn index_path(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

/// The docs.rs page of a crate. docs.rs redirects it to the rendered
/// docs of the crate's library target, whatever its lib name is.
pub(crate) fn docs_url(name: &str) -> String {
    format!("https://docs.rs/{name}")
}

/// Headers for a documented keyless API request: the honest
/// [`super::api_user_agent`] and `accept`. No browser profile, no
/// `sec-fetch-*`; `reqwest` adds `Accept-Encoding` for its enabled codecs.
pub(crate) fn apply_api_headers(
    builder: reqwest::RequestBuilder,
    accept: &str,
) -> reqwest::RequestBuilder {
    builder
        .header(reqwest::header::USER_AGENT, super::api_user_agent())
        .header(reqwest::header::ACCEPT, accept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_path_follows_cargo_layout() {
        let cases = [
            ("a", "1/a"),
            ("h2", "2/h2"),
            ("syn", "3/s/syn"),
            ("bytes", "by/te/bytes"),
            ("Serde_JSON", "se/rd/serde_json"),
        ];
        for (name, want) in cases {
            assert_eq!(index_path(name), want, "{name}");
        }
    }

    #[test]
    fn crate_name_rule() {
        for ok in ["bytes", "serde_json", "tokio-util", "a", "h2"] {
            assert!(is_crate_name(ok), "{ok}");
        }
        for bad in [
            "",
            "1password",
            "-x",
            "with space",
            "a.b",
            "../x",
            &"x".repeat(65),
        ] {
            assert!(!is_crate_name(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn gate_spaces_callers_by_the_gap() {
        let gate = ApiGate::new(Duration::from_millis(120));
        let start = Instant::now();
        gate.wait().await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "first is free"
        );
        gate.wait().await;
        gate.wait().await;
        assert!(start.elapsed() >= Duration::from_millis(240));
    }
}
