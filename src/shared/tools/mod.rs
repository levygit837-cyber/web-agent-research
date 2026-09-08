//! Agent tools: one implementation each, consumed by the slices.
//!
//! Context catalog: a tool is born here even with a single consumer
//! (e.g. `interact` only in `deep`); domain logic follows the 2+ rule.

pub mod cache;
pub mod fetch;
pub mod interact;
pub mod llm;
pub mod score_url;
pub mod search;
