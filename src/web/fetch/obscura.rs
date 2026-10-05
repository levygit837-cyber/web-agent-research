//! Obscura browser fallback for [`super::fetcher::Fetcher`].
//!
//! Single engine (subprocess today), spawned only when the static path in
//! `fetcher` decides the response is unusable (ADR-0006 §3). URL validation
//! lives in [`super::error::normalize_url`] so every caller gets it for free.
//!
//! Before spawning, the target host must resolve to public addresses only
//! ([`super::egress::check_resolved`], #97). stdout and stderr are read
//! incrementally, each capped at [`super::MAX_BODY_BYTES`]; the child is
//! killed as soon as either stream crosses the cap.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

use super::egress::{check_resolved, EgressPolicy};
use super::error::{normalize_url, FetchError};
use super::MAX_BODY_BYTES;

/// Concrete handle to the single Obscura engine (subprocess today).
/// `binary` is the internal seam tests use to inject the fixture double.
#[derive(Debug, Clone)]
pub struct Obscura {
    pub binary: PathBuf,
    pub timeout: Duration,
    /// Egress policy checked before spawning; `Fetcher` overrides it with
    /// its own via [`Obscura::with_egress`].
    egress: EgressPolicy,
}

/// Why reading the child's output stopped early.
enum ReadFailure {
    Io(std::io::Error),
    /// The named stream crossed [`MAX_BODY_BYTES`].
    Overflow(&'static str),
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
        Self {
            binary,
            timeout,
            egress: EgressPolicy::from_env(),
        }
    }

    /// The same handle with an explicit egress policy.
    pub fn with_egress(self, egress: EgressPolicy) -> Self {
        Self { egress, ..self }
    }

    /// Spawn `obscura fetch <normalized-url> --dump markdown --quiet`,
    /// collect stdout as markdown, classify failures per SPEC §3.
    /// Relies on CLI defaults: `--wait-until load`, adaptive settle when
    /// `--wait` is omitted. A fetch still running after `timeout` is killed
    /// and classified `Timeout`.
    pub async fn fetch_markdown(&self, raw_url: &str) -> Result<FetchedMarkdown, FetchError> {
        let normalized = normalize_url(raw_url, self.egress)?;
        let parsed = reqwest::Url::parse(&normalized).expect("normalize_url output parses");
        check_resolved(&parsed, self.egress)
            .await
            .map_err(|denied| FetchError::Egress {
                url: normalized.clone(),
                reason: denied.reason,
            })?;
        let mut child = tokio::process::Command::new(&self.binary)
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
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let collected = tokio::time::timeout(self.timeout, async {
            let (out, err) =
                tokio::try_join!(read_capped(stdout, "stdout"), read_capped(stderr, "stderr"))?;
            let status = child.wait().await.map_err(ReadFailure::Io)?;
            Ok::<_, ReadFailure>((status, out, err))
        })
        .await;
        let (status, stdout, stderr) = match collected {
            Err(_) => {
                return Err(FetchError::Timeout {
                    url: normalized,
                    after: self.timeout,
                });
            }
            Ok(Err(ReadFailure::Overflow(stream))) => {
                // Killed here rather than on drop so the process is gone
                // before the error reaches the caller.
                let _ = child.kill().await;
                return Err(FetchError::CommandFailed {
                    url: normalized,
                    detail: format!("obscura {stream} exceeds {MAX_BODY_BYTES} bytes; killed"),
                });
            }
            Ok(Err(ReadFailure::Io(e))) => {
                return Err(FetchError::CommandFailed {
                    url: normalized,
                    detail: e.to_string(),
                });
            }
            Ok(Ok(collected)) => collected,
        };

        let stderr = String::from_utf8_lossy(&stderr);
        let stderr_tail = tail(&stderr);
        if is_blocked(&stderr) {
            return Err(FetchError::Blocked {
                url: normalized,
                reason: stderr_tail,
            });
        }

        if !status.success() {
            return Err(FetchError::CommandFailed {
                url: normalized,
                detail: format!("exit {status}: {stderr_tail}"),
            });
        }

        let markdown = String::from_utf8_lossy(&stdout).trim().to_string();
        if markdown.is_empty() {
            return Err(FetchError::EmptyBody { url: normalized });
        }

        Ok(FetchedMarkdown {
            url: normalized,
            markdown: super::clean::clean_markdown(&markdown),
        })
    }
}

/// Read `stream` to its end, failing with [`ReadFailure::Overflow`] as soon
/// as more than [`MAX_BODY_BYTES`] arrive; never buffers past the cap by
/// more than one read.
async fn read_capped(
    mut stream: impl AsyncRead + Unpin,
    name: &'static str,
) -> Result<Vec<u8>, ReadFailure> {
    let mut out = Vec::new();
    // On the heap: two of these live in the caller's future at once, and a
    // stack array would inflate every future that awaits a fetch.
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = stream.read(&mut buf).await.map_err(ReadFailure::Io)?;
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n > MAX_BODY_BYTES {
            return Err(ReadFailure::Overflow(name));
        }
        out.extend_from_slice(&buf[..n]);
    }
}

impl Default for Obscura {
    /// `obscura` on PATH, [`DEFAULT_TIMEOUT`], egress policy from the env.
    fn default() -> Self {
        Self::new("obscura".into(), DEFAULT_TIMEOUT)
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
