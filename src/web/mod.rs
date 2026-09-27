//! Web layer: search (Hits) and fetch (Evidence). No LLM, no Session.
//!
//! ADR-0006: `research` depends on `web`; `web` never imports `research` or `llm`.

pub mod fetch;
pub mod search;
