//! Research agent: loop, prompt, Synthesis, Session JSONL, and tool dispatch.
//!
//! ADR-0006: the only module that knows Research/Turn; consumes `web` and `llm`.

pub mod agent_loop;
pub mod prompt;
pub mod run;
pub mod session;
pub mod synthesis;

pub use run::{
    render, run_research, CitationDTO, ResearchError, ResearchRequest, ResearchResponse,
    SynthesisDTO, ThemeDTO, UsageDTO,
};
