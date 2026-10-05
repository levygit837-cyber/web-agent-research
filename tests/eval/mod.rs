//! Offline evaluation harness (#107). Eval-only: shared by the
//! `examples/eval.rs` driver and the `tests/eval_*.rs` gates, never part of
//! the shipped binary. See `docs/eval.md`.
//!
//! - [`golden`]: the golden set (`tests/golden/*.json`).
//! - [`metrics`]: one [`metrics::RunRow`] per run from the `--json` stdout.
//! - [`transcript`]: the recording gateway proxy and the replaying stub.
//! - [`runner`]: run the CLI once per goal and repeat, in a tape mode.
//! - [`judge`]: SimpleQA grade plus per-nugget support, via the gateway.
//! - [`results`]: the results file and its per-category summary.
//! - [`compare`]: paired comparison of two results files.
//! - [`pack`]: gzip fixture dirs for the repo and unpack them for tests.

pub mod compare;
pub mod golden;
pub mod judge;
pub mod metrics;
pub mod pack;
pub mod results;
pub mod runner;
pub mod transcript;
