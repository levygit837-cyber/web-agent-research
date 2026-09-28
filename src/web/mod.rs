//! Web layer: search (Hits) and fetch (Evidence). No LLM, no Session.
//!
//! ADR-0006: `research` depends on `web`; `web` never imports `research` or `llm`.

pub(crate) mod cache_dir;
pub mod fetch;
pub(crate) mod profile;
pub mod search;

/// Browser-like User-Agent for the fetch static path (`web::fetch::fetcher`)
/// only; some hosts serve empty shells or block unknown agents without one.
/// The search legs (`web::search::ddg`/`startpage`) send a Chrome
/// 153-155 UA from `web::profile` instead (#59), not this constant.
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// `Accept-Language` header value shared by every `web/` HTTP client.
pub(crate) const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";
