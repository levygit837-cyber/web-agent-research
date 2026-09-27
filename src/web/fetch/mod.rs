//! Fetch: URL → Evidence. `reqwest` + `htmd` first, Obscura fallback (ADR-0006 §3).

pub mod error;
pub mod evidence;
pub mod fetcher;
pub mod obscura;
pub mod tool;

pub use error::FetchError;
pub use evidence::Evidence;
pub use fetcher::{FetchPath, Fetcher};
pub use obscura::{FetchedMarkdown, Obscura};
