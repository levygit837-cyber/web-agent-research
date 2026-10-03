//! Fetch failure modes shared by the static path ([`super::fetcher::Fetcher`])
//! and the Obscura browser fallback ([`super::obscura::Obscura`]).

use std::fmt;
use std::time::Duration;

/// Parse `raw_url` with `reqwest::Url`, requiring an absolute URL whose
/// scheme is `http` or `https`. Returns the re-serialized canonical URL.
pub(crate) fn normalize_url(raw_url: &str) -> Result<String, FetchError> {
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
    /// `part` is not an integer >= 1. No request was made. `input` is the
    /// compact JSON of the supplied value.
    InvalidPart { input: String },
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
    /// Static HTTP failure a browser would not fix: 404/5xx, transport error,
    /// oversize body, or a 403/429/503 without a challenge marker.
    Http { url: String, detail: String },
    /// Browser needed (or requested) but the Obscura binary is missing.
    /// `reason` says why the static path was not enough.
    FallbackUnavailable { url: String, reason: String },
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
            FetchError::InvalidPart { input } => {
                write!(f, "invalid part: {input} (expected an integer >= 1)")
            }
            FetchError::Http { url, detail } => write!(f, "http error: {url}: {detail}"),
            FetchError::FallbackUnavailable { url, reason } => write!(
                f,
                "browser fallback unavailable: {url}: {reason}; install obscura on PATH"
            ),
        }
    }
}

impl std::error::Error for FetchError {}
