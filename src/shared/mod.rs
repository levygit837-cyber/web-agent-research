//! Shared: `tools/` and `prompts/` are context catalogs
//! (born here even with a single consumer); the rest follows the 2+ rule:
//! born for one Mode, stays in the slice; served a second, moves here.
pub mod prompts;
pub mod session;
pub mod tools;
pub mod types;
