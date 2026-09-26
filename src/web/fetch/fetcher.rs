//! URL → markdown: `reqwest` + `htmd` first, Obscura only as fallback (ADR-0006 §3).
//!
//! Fallback triggers: HTTP 403/429/503, a challenge page on 200, markdown
//! shorter than `min_markdown_chars` (JS shell), or a content type that is
//! neither HTML nor text. Other HTTP errors (404, 500, …) return
//! `FetchError::Http` without spawning the browser: Obscura would see the
//! same status.

use std::time::Duration;

use super::obscura::{normalize_url, FetchError, FetchedMarkdown, Obscura};

/// Which engine produced the markdown. Debug-only; not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPath {
    Static,
    Browser,
}

/// Browser-like UA; some hosts serve empty shells to unknown agents.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Tags whose text is never content (`head` covers `title`/`meta`/`style`).
const SKIP_TAGS: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "iframe", "nav", "footer", "form",
];

/// Lowercase markers of anti-bot interstitials served with HTTP 200.
const CHALLENGE_MARKERS: &[&str] = &[
    "cf-chl-",
    "challenge-platform",
    "just a moment...",
    "attention required! | cloudflare",
    "enable javascript and cookies to continue",
];

/// Static HTTP fetch with Obscura fallback. Cheap to clone; share one per run.
#[derive(Debug, Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    obscura: Obscura,
    /// Response bodies above this are rejected before conversion.
    max_body_bytes: usize,
    /// Static markdown shorter than this (trimmed chars) is treated as a JS shell.
    min_markdown_chars: usize,
}

/// Why the static path handed over to Obscura; kept for the error message.
enum Handover {
    Status(u16),
    Challenge,
    ThinMarkdown(usize),
    ContentType(String),
}

impl std::fmt::Display for Handover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(code) => write!(f, "HTTP {code}"),
            Self::Challenge => write!(f, "challenge page"),
            Self::ThinMarkdown(len) => write!(f, "only {len} chars of markdown (JS shell?)"),
            Self::ContentType(ct) => write!(f, "unsupported content type {ct}"),
        }
    }
}

enum StaticOutcome {
    Done(FetchedMarkdown),
    Fallback(Handover),
}

impl Fetcher {
    /// Production defaults: 15 s static timeout, 5 MiB body cap, 200-char
    /// JS-shell threshold, `obscura` on PATH with a 60 s timeout.
    pub fn new() -> Self {
        Self::with_obscura(Obscura::default())
    }

    /// Same defaults with an injected Obscura handle (tests use the fixture double).
    pub fn with_obscura(obscura: Obscura) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .expect("static reqwest client config is valid");
        Self {
            client,
            obscura,
            max_body_bytes: 5 * 1024 * 1024,
            min_markdown_chars: 200,
        }
    }

    /// Fetch `raw_url` as markdown. Tries the static path; falls back to
    /// Obscura on the triggers listed in the module docs.
    pub async fn fetch(&self, raw_url: &str) -> Result<(FetchedMarkdown, FetchPath), FetchError> {
        let url = normalize_url(raw_url)?;
        let handover = match self.fetch_static(&url).await? {
            StaticOutcome::Done(page) => return Ok((page, FetchPath::Static)),
            StaticOutcome::Fallback(handover) => handover,
        };
        tracing::debug!(%url, reason = %handover, "static fetch unusable, falling back to obscura");
        match self.obscura.fetch_markdown(&url).await {
            Ok(page) => Ok((page, FetchPath::Browser)),
            Err(FetchError::FallbackUnavailable { .. }) => Err(FetchError::FallbackUnavailable {
                url,
                reason: handover.to_string(),
            }),
            Err(err) => Err(err),
        }
    }

    async fn fetch_static(&self, url: &str) -> Result<StaticOutcome, FetchError> {
        let response = self
            .client
            .get(url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
            .map_err(|err| {
                if err.is_timeout() {
                    FetchError::Timeout {
                        url: url.to_owned(),
                        after: Duration::from_secs(15),
                    }
                } else {
                    FetchError::Http {
                        url: url.to_owned(),
                        detail: err.to_string(),
                    }
                }
            })?;

        let status = response.status().as_u16();
        if matches!(status, 403 | 429 | 503) {
            return Ok(StaticOutcome::Fallback(Handover::Status(status)));
        }
        if !response.status().is_success() {
            return Err(FetchError::Http {
                url: url.to_owned(),
                detail: format!("HTTP {status}"),
            });
        }
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("text/html")
            .to_ascii_lowercase();
        if response
            .content_length()
            .is_some_and(|len| len as usize > self.max_body_bytes)
        {
            return Err(self.too_large(url));
        }
        let body = response.bytes().await.map_err(|err| FetchError::Http {
            url: url.to_owned(),
            detail: err.to_string(),
        })?;
        if body.len() > self.max_body_bytes {
            return Err(self.too_large(url));
        }
        let text = String::from_utf8_lossy(&body);

        let markdown = if content_type.contains("html") {
            if is_challenge(&text) {
                return Ok(StaticOutcome::Fallback(Handover::Challenge));
            }
            html_to_markdown(&text)
        } else if content_type.starts_with("text/") || content_type.contains("json") {
            text.trim().to_owned()
        } else {
            return Ok(StaticOutcome::Fallback(Handover::ContentType(content_type)));
        };

        let chars = markdown.chars().count();
        if chars < self.min_markdown_chars {
            return Ok(StaticOutcome::Fallback(Handover::ThinMarkdown(chars)));
        }
        Ok(StaticOutcome::Done(FetchedMarkdown {
            url: final_url,
            markdown,
        }))
    }

    fn too_large(&self, url: &str) -> FetchError {
        FetchError::Http {
            url: url.to_owned(),
            detail: format!("body exceeds {} bytes", self.max_body_bytes),
        }
    }
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

fn is_challenge(html: &str) -> bool {
    // Interstitials are small; only scan the head of large pages.
    let head: String = html.chars().take(20_000).collect::<String>().to_lowercase();
    CHALLENGE_MARKERS.iter().any(|marker| head.contains(marker))
}

/// HTML → trimmed markdown. Conversion errors (malformed input the parser
/// rejects) degrade to empty, which the caller treats as a JS shell.
fn html_to_markdown(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIP_TAGS.to_vec())
        .build()
        .convert(html)
        .map(|md| md.trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_content_and_drops_chrome() {
        let md = html_to_markdown(
            "<html><head><style>p{}</style></head><body><nav>Menu</nav><h1>Title</h1>\
             <p>Body <a href=\"https://x.test/\">link</a></p><script>var a;</script>\
             <footer>Foot</footer></body></html>",
        );
        assert!(md.starts_with("# Title"), "{md}");
        assert!(md.contains("[link](https://x.test/)"), "{md}");
        for dropped in ["Menu", "Foot", "var a", "p{}"] {
            assert!(!md.contains(dropped), "{dropped} leaked: {md}");
        }
    }

    #[test]
    fn detects_cloudflare_interstitial() {
        assert!(is_challenge(
            "<html><title>Just a moment...</title><div id=\"cf-chl-widget\"></div></html>"
        ));
        assert!(!is_challenge("<html><title>Rust docs</title></html>"));
    }
}
