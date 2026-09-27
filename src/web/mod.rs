//! Web layer: search (Hits) and fetch (Evidence). No LLM, no Session.
//!
//! ADR-0006: `research` depends on `web`; `web` never imports `research` or `llm`.

pub mod fetch;
pub mod search;

/// Browser-like User-Agent shared by every `web/` HTTP client (search legs
/// and the fetch static path); some hosts serve empty shells or block
/// unknown agents without it.
pub(crate) const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// `Accept-Language` header value shared by every `web/` HTTP client.
pub(crate) const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";
