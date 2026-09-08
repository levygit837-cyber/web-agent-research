//! Obscura browser functions backing search and fetch.
//!
//! Single engine (subprocess today). URL validation lives here so every
//! caller gets it for free; the fetch tool delegates to [`Obscura`].

use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Concrete handle to the single Obscura engine (subprocess today).
/// `binary` is the internal seam tests use to inject the fixture double.
#[derive(Debug, Clone)]
pub struct Obscura {
    pub binary: PathBuf,
    pub timeout: Duration,
}

impl Obscura {
    /// Production handle: `obscura` on PATH, 60 s per-fetch timeout (headroom
    /// over the CLI's own 30 s navigation ceiling plus the 5 s adaptive settle,
    /// mirroring `scrape`'s 60 s per-URL precedent).
    pub fn new(binary: PathBuf, timeout: Duration) -> Self {
        Self { binary, timeout }
    }

    /// Spawn `obscura fetch <normalized-url> --dump markdown --quiet`,
    /// collect stdout as markdown, classify failures per SPEC §3.
    /// Relies on CLI defaults: `--wait-until load`, adaptive settle when
    /// `--wait` is omitted. The 60 s engine timeout stays above the CLI's
    /// internal 30 s navigation ceiling + 5 s settle so a slow-but-healthy
    /// fetch is not misclassified as `Timeout`.
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
            .map_err(|e| FetchError::CommandFailed {
                url: normalized.clone(),
                detail: e.to_string(),
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
            markdown,
        })
    }
}

impl Default for Obscura {
    /// `Obscura { binary: "obscura".into(), timeout: Duration::from_secs(60) }`.
    fn default() -> Self {
        Self {
            binary: "obscura".into(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// Trimmed markdown plus the normalized URL it was fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedMarkdown {
    pub url: String,
    pub markdown: String,
}

/// Parse `raw_url` with `reqwest::Url`, requiring an absolute URL whose
/// scheme is `http` or `https`. Returns the re-serialized canonical URL.
fn normalize_url(raw_url: &str) -> Result<String, FetchError> {
    match reqwest::Url::parse(raw_url) {
        Ok(url) if url.scheme() == "http" || url.scheme() == "https" => Ok(url.to_string()),
        _ => Err(FetchError::InvalidUrl {
            input: raw_url.to_string(),
        }),
    }
}

/// Every failure mode of `fetch_markdown`, and of the fetch tool by delegation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Input is not an absolute http(s) URL. No subprocess was spawned.
    /// `input` is the raw caller-supplied string.
    InvalidUrl { input: String },
    /// Exit 0 but stdout was empty/whitespace-only.
    EmptyBody { url: String },
    /// Engine reported access denial (exit code or stderr markers, §3).
    /// `reason` is the trimmed stderr tail (max 500 chars).
    Blocked { url: String, reason: String },
    /// No output within the engine timeout; the child was killed.
    Timeout { url: String, after: Duration },
    /// Spawn failure or any other non-zero exit / I/O error.
    /// `detail` carries the OS error or `status + stderr tail` (max 500 chars).
    CommandFailed { url: String, detail: String },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::InvalidUrl { input } => {
                write!(f, "invalid url: {input} (expected absolute http(s) URL)")
            }
            FetchError::EmptyBody { url } => {
                write!(f, "empty body: {url} returned no markdown")
            }
            FetchError::Blocked { url, reason } => write!(f, "blocked: {url}: {reason}"),
            FetchError::Timeout { url, after } => write!(f, "timeout: {url} after {after:?}"),
            FetchError::CommandFailed { url, detail } => {
                write!(f, "command failed: {url}: {detail}")
            }
        }
    }
}

impl std::error::Error for FetchError {}

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
