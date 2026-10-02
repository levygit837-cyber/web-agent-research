//! Research agent core (Research → Session → Turn → Evidence).
//!
//! Public interface: `run_research` (ADR-0006). `src/main.rs` is a thin CLI
//! shell over it; module layout and dependency rule live in ADR-0006.

/// Session JSONL format version. Breaking compat requires bump + migration.
pub const SESSION_FORMAT_VERSION: u32 = 1;

pub mod llm;
pub mod research;
#[cfg(test)]
pub(crate) mod test_support;
pub mod web;

pub use research::{
    render, run_research, CitationDTO, ResearchError, ResearchRequest, ResearchResponse,
    SynthesisDTO, ThemeDTO, UsageDTO,
};
