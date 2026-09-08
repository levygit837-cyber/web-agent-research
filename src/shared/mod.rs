//! Shared: `tools/`, `prompts/` and `types/` are context catalogs
//! (born here even with a single consumer); `obscura/`, `agent_loop/` and
//! `web_engine_search/` are engines every slice consumes; the rest follows
//! the 2+ rule:
//! born for one Mode, stays in the slice; served a second, moves here.
pub mod agent_loop;
pub mod gateway;
pub mod obscura;
pub mod prompts;
pub mod session;
pub mod tools;
pub mod types;
pub mod web_engine_search;
