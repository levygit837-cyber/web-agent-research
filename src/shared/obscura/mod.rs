//! Obscura engine: browser functions every Mode consumes.
//!
//! Single engine (subprocess today, ported code tomorrow). `shared/`
//! modules may consume each other; slices consume `shared/`, never siblings.

pub mod browser;
