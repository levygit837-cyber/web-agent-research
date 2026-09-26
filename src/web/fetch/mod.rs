//! Fetch: URL → Evidence. Obscura subprocess today; reqwest-first lands in #31.

pub mod evidence;
pub mod obscura;
pub mod tool;

pub use evidence::Evidence;
pub use obscura::{FetchError, FetchedMarkdown, Obscura};
