//! Shared: `tools/`, `prompts/`, `types/` and `schemas/` are context
//! catalogs (born here even with a single consumer); `obscura/` and
//! `agent_loop/` are engines every slice consumes; the rest follows
//! the 2+ rule: born for one Mode, stays in the slice; served a second,
//! moves here.
pub mod agent_loop;
pub mod obscura;
pub mod prompts;
pub mod schemas;
pub mod session;
pub mod tools;
pub mod types;
