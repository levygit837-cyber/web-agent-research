//! `search` Mode: agent loop over search → fetch → synthesize.

pub mod handler;

pub use handler::{
    render, run_research, CitationDTO, ResearchError, ResearchRequest, ResearchResponse,
    SynthesisDTO, ThemeDTO, UsageDTO,
};
