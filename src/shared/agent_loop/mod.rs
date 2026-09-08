//! Multi-turn agent loop: LLM → tool → ground truth → re-plan.
//!
//! Every Mode runs on this loop; Modes differ by budget and tools allowed.
//! The external seam is `run_loop` plus `LoopInput`, `LoopBudget`,
//! `RunReport`, `ToolEvidence`, and `LoopError` (see `runner.rs`).

pub mod context;
pub mod registry;
pub mod runner;
pub mod synthesis;

pub use registry::{ToolRegistry, ToolResult};
pub use runner::{run_loop, LoopBudget, LoopError, LoopInput, RunReport, ToolEvidence};
