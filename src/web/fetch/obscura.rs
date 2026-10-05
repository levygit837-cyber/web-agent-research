//! Obscura browser fallback for [`super::fetcher::Fetcher`].
//!
//! Single engine (subprocess today), spawned only when the static path in
//! `fetcher` decides the response is unusable (ADR-0006 §3). URL validation
//! lives in [`super::error::normalize_url`] so every caller gets it for free.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use super::error::{normalize_url, FetchError};

/// Concrete handle to the single Obscura engine (subprocess today).
/// `binary` is the internal seam tests use to inject the fixture double.
#[derive(Debug, Clone)]
pub struct Obscura {
    pub binary: PathBuf,
    pub timeout: Duration,
}

/// Production per-fetch timeout. Sized so a static attempt that times out
/// (10 s) plus this fallback fits inside the agent loop's 45 s per-tool
/// timeout with 5 s to spare (#100); the agent loop asserts the fit. It
/// matches the CLI's own 30 s navigation ceiling, so a page that needs the
/// ceiling plus the adaptive settle is reported as `Timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

impl Obscura {
    /// Handle over `binary` with a per-fetch `timeout` (production uses
    /// [`Obscura::default`]: `obscura` on PATH, [`DEFAULT_TIMEOUT`]).
    pub fn new(binary: PathBuf, timeout: Duration) -> Self {
        Self { binary, timeout }
    }

    /// Spawn `obscura fetch <normalized-url> --dump markdown --quiet`,
    /// collect stdout as markdown, classify failures per SPEC §3.
    /// Relies on CLI defaults: `--wait-until load`, adaptive settle when
    /// `--wait` is omitted. A fetch still running after `timeout` is killed
    /// and classified `Timeout`.
    pub async fn fetch_markdown(&self, raw_url: &str) -> Result<FetchedMarkdown, FetchError> {
        let normalized = normalize_url(raw_url)?;
        let child = tokio::process::Command::new(&self.binary)
            .arg("fetch")
            .arg(&normalized)
            .arg("--dump")
            .arg("markdown")
            .arg("--quiet")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => FetchError::FallbackUnavailable {
                    url: normalized.clone(),
                    reason: format!("{} not found", self.binary.display()),
                },
                _ => FetchError::CommandFailed {
                    url: normalized.clone(),
                    detail: e.to_string(),
                },
            })?;

        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Err(_) => {
                return Err(FetchError::Timeout {
                    url: normalized,
                    after: self.timeout,
                });
            }
            Ok(Err(e)) => {
                return Err(FetchError::CommandFailed {
                    url: normalized,
                    detail: e.to_string(),
                });
            }
            Ok(Ok(output)) => output,
        };

        let stderr_tail = tail(&String::from_utf8_lossy(&output.stderr));
        if is_blocked(&String::from_utf8_lossy(&output.stderr)) {
            return Err(FetchError::Blocked {
                url: normalized,
                reason: stderr_tail,
            });
        }

        if !output.status.success() {
            return Err(FetchError::CommandFailed {
                url: normalized,
                detail: format!("exit {}: {}", output.status, stderr_tail),
            });
        }

        let markdown = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if markdown.is_empty() {
            return Err(FetchError::EmptyBody { url: normalized });
        }

        Ok(FetchedMarkdown {
            url: normalized,
            markdown: super::clean::clean_markdown(&markdown),
        })
    }
}

impl Default for Obscura {
    /// `Obscura { binary: "obscura".into(), timeout: DEFAULT_TIMEOUT }`.
    fn default() -> Self {
        Self {
            binary: "obscura".into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Trimmed markdown plus the normalized URL it was fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedMarkdown {
    pub url: String,
    pub markdown: String,
}

/// Plain substring markers (case-insensitive). `403` and `bot` are checked
/// separately with boundary guards (see [`contains_guarded`]).
const BLOCKED_MARKERS: &[&str] = &[
    "forbidden",
    "access denied",
    "blocked",
    "captcha",
    "rate limit",
    "rate-limited",
    "too many requests",
];

/// True when `stderr` carries a blocked marker (case-insensitive, checked
/// regardless of exit code). `403` matches only with non-digit boundaries on
/// both sides (so `14034` never matches); `bot` matches only with non-letter
/// boundaries on both sides (so `robots` chatter never matches).
fn is_blocked(stderr: &str) -> bool {
    let lowered = stderr.to_lowercase();
    if BLOCKED_MARKERS.iter().any(|m| lowered.contains(m)) {
        return true;
    }
    contains_guarded(&lowered, "403", u8::is_ascii_digit)
        || contains_guarded(&lowered, "bot", u8::is_ascii_alphabetic)
}

/// True when `needle` occurs in `haystack` with at least one occurrence whose
/// immediate neighbors (or string edges) are not `is_boundary_char`.
/// Std-only boundary scan; no regex crate.
fn contains_guarded(haystack: &str, needle: &str, is_boundary_char: fn(&u8) -> bool) -> bool {
    let hay = haystack.as_bytes();
    let ndl = needle.as_bytes();
    if ndl.is_empty() || hay.len() < ndl.len() {
        return false;
    }
    hay.windows(ndl.len())
        .enumerate()
        .filter(|(_, w)| *w == ndl)
        .any(|(i, _)| {
            let left_ok = i == 0 || !is_boundary_char(&hay[i - 1]);
            let right_ok = i + ndl.len() == hay.len() || !is_boundary_char(&hay[i + ndl.len()]);
            left_ok && right_ok
        })
}

/// Trimmed stderr tail, at most 500 chars (char-boundary safe).
fn tail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.chars().count() <= 500 {
        return trimmed.to_string();
    }
    trimmed
        .chars()
        .skip(trimmed.chars().count() - 500)
        .collect()
}
