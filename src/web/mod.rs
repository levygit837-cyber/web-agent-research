//! Web layer: search (Hits) and fetch (Evidence). No LLM, no Session.
//!
//! ADR-0006: `research` depends on `web`; `web` never imports `research` or `llm`.

pub(crate) mod cache_dir;
pub mod fetch;
pub(crate) mod profile;
pub mod search;

/// `Accept-Language` header value shared by every `web/` HTTP client.
pub(crate) const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";
