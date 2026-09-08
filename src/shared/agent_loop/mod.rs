//! Multi-turn agent loop: LLM → tool → ground truth → re-plan.
//!
//! Every Mode runs on this loop; Modes differ by budget and tools allowed.

pub mod runner;
