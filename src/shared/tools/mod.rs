//! Agent tools: one implementation each, consumed by the slices.
//!
//! Executable behavior only (`search`, `fetch`). Scoring and caching
//! are behaviors inside tools, not tools. LLM is not a tool: it drives
//! the `agent_loop/` via the OpenAI-compatible gateway (see ADR-0003).

pub mod fetch;
pub mod search;
