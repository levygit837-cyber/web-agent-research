//! Multi-turn agent loop: LLM → tool → ground truth → re-plan.
//!
//! Modes differ by budget and tools allowed.
//! The external seam is `run_loop` plus `LoopInput`, `LoopBudget`,
//! `RunReport`, `ToolEvidence`, and `LoopError` (see `runner.rs` for the
//! loop, `types.rs` for the contract types).

pub mod answer;
pub mod context;
mod dispatch;
pub mod registry;
pub mod result;
pub mod runner;
#[cfg(test)]
pub(crate) mod test_support;
pub mod types;

pub use registry::ToolRegistry;
pub use result::ToolResult;
pub use runner::run_loop;
pub use types::{LoopBudget, LoopError, LoopInput, RunReport, ToolEvidence};
