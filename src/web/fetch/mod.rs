//! Fetch: URL → Evidence. `reqwest` + `htmd` first, Obscura fallback (ADR-0006 §3).

pub(crate) mod clean;
pub mod egress;
pub mod error;
pub mod evidence;
pub(crate) mod extract;
pub mod fetcher;
pub mod obscura;
pub mod tool;

pub use egress::EgressPolicy;
pub use error::FetchError;
pub use evidence::Evidence;
pub use fetcher::{FetchPath, Fetcher};
pub use obscura::{FetchedMarkdown, Obscura};

/// Byte cap on one page from either path: the static response body
/// (decoded) and Obscura's stdout and stderr each (#97).
pub(crate) const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
