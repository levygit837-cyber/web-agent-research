//! Fetch-only web search engine: DuckDuckGo, Startpage, Brave, Yahoo, Bing
//! over plain HTTP (#66), plus the crates.io vertical over its keyless
//! JSON API (#109).
//!
//! Small Interface (`search_multi_with_bases`, pure helpers, per-engine
//! `#[doc(hidden)]` base-URL overrides) over provider legs (`ddg`,
//! `startpage`, `brave`, `yahoo`, `bing`, `cratesio`), merge (`dedup`),
//! fan-out (`fanout`) and codecs (`decode`). Callers cross only this root;
//! provider forms and deadlines stay inside.
//!
//! `ChainPosition`'s `sec-fetch-site` derivation ports Obscura's
//! `request_fetch_site` (Apache-2.0, h4ckf0r0day/obscura@542df14); see
//! `THIRD-PARTY-NOTICES.md`. `apply_navigation_headers`'s own header order
//! is independently verified against live Chrome, not ported (see its doc).

pub mod bing;
pub mod brave;
pub(crate) mod cache;
mod coverage;
pub mod cratesio;
pub mod ddg;
pub mod decode;
pub mod dedup;
mod dispatch;
pub mod fanout;
pub(crate) mod governor;
mod markup;
pub mod startpage;
#[cfg(test)]
pub(crate) mod test_support;
pub mod tool;
pub mod types;
pub mod yahoo;
#[doc(hidden)]
pub use bing::bing_search_with_base;
pub use bing::{is_bing_challenge, parse_bing_html, unwrap_bing_url, BING_SEARCH_URL};
#[doc(hidden)]
pub use brave::brave_search_with_base;
pub use brave::{parse_brave_html, BRAVE_SEARCH_URL};
#[doc(hidden)]
pub use cratesio::cratesio_search_with_base;
pub use cratesio::CRATESIO_SEARCH_URL;
#[doc(hidden)]
pub use ddg::ddg_search_with_base;
pub use ddg::{
    is_ddg_anomaly, parse_continuation_form, parse_ddg_html, unwrap_ddg_url, DDG_HTML_URL,
    DDG_REFERER,
};
pub use dedup::{dedup_key, merge_sources};
pub use fanout::all_failed_message;
#[doc(hidden)]
pub use fanout::search_multi_with_bases;
#[doc(hidden)]
pub use governor::Governor;
#[doc(hidden)]
pub use startpage::startpage_search_with_base;
pub use startpage::{
    is_startpage_challenge, parse_search_form_inputs, parse_startpage_html, sanitize_startpage_url,
    STARTPAGE_HOME_URL, STARTPAGE_SEARCH_URL,
};
#[doc(hidden)]
pub use yahoo::yahoo_search_with_base;
pub use yahoo::{is_yahoo_challenge, parse_yahoo_html, unwrap_yahoo_url, YAHOO_SEARCH_URL};

/// Position of one HTTP call inside a request chain (#59: "one engine leg
/// plus its follow-ups"). Chrome's `sec-fetch-site` is `none` only for the
/// first request; a same-origin follow-up (DDG's `s`/`vqd` continuation
/// re-POST, the Startpage homepage -> search POST, or its direct-GET
/// fallback) sends `same-origin` (port of Obscura `request_fetch_site`,
/// `crates/obscura-net/src/client.rs:438-450`, Apache-2.0,
/// h4ckf0r0day/obscura@542df14; see `THIRD-PARTY-NOTICES.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainPosition {
    First,
    FollowUp,
}

impl ChainPosition {
    fn sec_fetch_site(self) -> &'static str {
        match self {
            ChainPosition::First => "none",
            ChainPosition::FollowUp => "same-origin",
        }
    }
}

/// Chrome navigation header order (#58): `sec-ch-ua*`,
/// `upgrade-insecure-requests`, `user-agent`, `accept`, `sec-fetch-*`,
/// `accept-encoding`, `accept-language`, then `referer`/`origin`/
/// `content-type` when the caller has them. Verified directly against a
/// live Chrome capture via `https://tls.peet.ws/api/all` (see the PR body)
/// and matches the issue's acceptance order.
///
/// This deliberately does NOT reuse Obscura's insertion order verbatim
/// (`crates/obscura-net/src/client.rs:1487-1498`, Apache-2.0,
/// h4ckf0r0day/obscura@542df14), which puts `referer` right after
/// `sec-fetch-dest`, before `accept-language`: Obscura never sets an
/// explicit `Accept-Encoding` and instead lets reqwest append it after
/// `accept-language` as a side effect of its own header-insertion quirk
/// (their comment on the same lines says as much). #57 requires an
/// explicit `Accept-Encoding`, which removes that quirk and lets this
/// function place `accept-encoding` where live Chrome actually sends it --
/// right after `sec-fetch-dest`, ahead of `referer`. Only the client-hints
/// GREASE algorithm (`chrome_client_hints`, `profile.rs`) and the
/// `sec-fetch-site` derivation (`ChainPosition`) are ported from Obscura.
/// `reqwest` still inserts its own `Host` after every header this
/// function sets -- on HTTP/1.1 that lands last on the wire regardless, and
/// there is no earlier seam to move it without the #56 transport.
pub(crate) fn apply_navigation_headers(
    builder: reqwest::RequestBuilder,
    profile: &crate::web::profile::BrowserProfile,
    position: ChainPosition,
    referer: Option<&str>,
) -> reqwest::RequestBuilder {
    let builder = builder
        .header("sec-ch-ua", &profile.sec_ch_ua)
        .header("sec-ch-ua-mobile", profile.sec_ch_ua_mobile)
        .header("sec-ch-ua-platform", profile.sec_ch_ua_platform)
        .header("upgrade-insecure-requests", "1")
        .header("user-agent", &profile.user_agent)
        .header("accept", profile.accept)
        .header("sec-fetch-site", position.sec_fetch_site())
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-user", "?1")
        .header("sec-fetch-dest", "document")
        .header("accept-encoding", profile.accept_encoding)
        .header("accept-language", profile.accept_language);
    match referer {
        Some(referer) => builder.header("Referer", referer),
        None => builder,
    }
}

/// Headers for a documented keyless API request (#109: crates.io search
/// and the crate-page fetch rewrite): the honest
/// [`crate::web::api_user_agent`] naming the application and a contact,
/// and `accept`. The plain counterpart of [`apply_navigation_headers`]:
/// no browser profile, no client hints, no `sec-fetch-*`; `reqwest` adds
/// `Accept-Encoding` for its enabled codecs.
pub(crate) fn apply_api_headers(
    builder: reqwest::RequestBuilder,
    accept: &str,
) -> reqwest::RequestBuilder {
    builder
        .header(reqwest::header::USER_AGENT, crate::web::api_user_agent())
        .header(reqwest::header::ACCEPT, accept)
}

/// `scheme://host[:port]` of `url`, for the `Origin` header on same-origin
/// form POSTs: Chrome sends `Origin` on POST/PUT/PATCH/DELETE even
/// same-origin (MDN "Origin header": "user agents add [it] to ... same-origin
/// requests except for GET or HEAD requests"). `None` on an unparseable URL
/// (never sent, rather than sending a wrong value).
pub(crate) fn request_origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

/// Per-request timeout ceiling: `LEG_TIMEOUT_SECS` in production, 1 s under
/// `cfg(test)` so the slow-stub leg-timeout test runs in milliseconds while
/// still exercising the real `.timeout(...)` path in `ddg_post` and friends.
pub(crate) fn leg_timeout() -> std::time::Duration {
    #[cfg(test)]
    return std::time::Duration::from_secs(1);
    #[cfg(not(test))]
    return std::time::Duration::from_secs(crate::web::search::types::LEG_TIMEOUT_SECS);
}
