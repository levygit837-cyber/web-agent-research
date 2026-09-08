//! Research agent core (Research → Session → Turn → Evidence).
//!
//! The binary in `src/main.rs` is a thin CLI shell; all domain logic
//! lives here so the future HTTP API (Axum) can reuse it without rewrite.

/// Session JSONL format version. Breaking compat requires bump + migration.
pub const SESSION_FORMAT_VERSION: u32 = 1;

pub mod shared;
pub mod slices;

pub use slices::search::{
    render, run_research, CitationDTO, ResearchError, ResearchRequest, ResearchResponse,
    SynthesisDTO, ThemeDTO, UsageDTO,
};
