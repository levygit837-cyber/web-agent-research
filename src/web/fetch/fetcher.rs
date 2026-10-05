//! URL → markdown: `reqwest` + `htmd` first, Obscura only as fallback (ADR-0006 §3).
//!
//! Fallback triggers: HTTP 403/429/503 whose body (or the Cloudflare
//! `cf-mitigated: challenge` header) marks it as a challenge page, a
//! challenge page on 200, markdown shorter than `min_markdown_chars` (JS
//! shell), or a content type that is neither HTML nor text. A 403/429/503
//! *without* a challenge marker, and other HTTP errors (404, 500, …), return
//! `FetchError::Http` without spawning the browser: Obscura would see the
//! same status (or the same absence of one).
//!
//! Each static-path request draws one Chrome browser profile
//! (`web::profile::pick_profile`, #74) and sends it through
//! `web::search::apply_navigation_headers` as a direct navigation
//! (`ChainPosition::First`, no Referer): `sec-fetch-site: none`, the
//! profile's coherent UA/`sec-ch-ua`/client-hint set, and its
//! `Accept-Encoding` (#57), all in Chrome's real navigation header order.
//! `reqwest`'s `gzip`/`brotli`/`zstd`/`deflate` features decode the response
//! body transparently. Decompression removes `Content-Length` before
//! `read_capped_body` sees the response, so the cap always applies to
//! decoded bytes, streamed chunk-by-chunk, never to the compressed size.
//!
//! Egress (#97): the client resolves names through
//! [`super::egress::EgressResolver`] and re-checks every redirect hop with
//! [`super::egress::redirect_policy`], so no request reaches a private or
//! reserved address unless the policy is [`EgressPolicy::AllowPrivate`].
//!
//! HTML bodies keep only their main-content subtree when
//! [`super::extract::keep_extracted`] says nothing is lost (#108).

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::egress::{denied_in_chain, redirect_policy, EgressPolicy, EgressResolver};
use super::error::{normalize_url, FetchError};
use super::extract::{extract, keep_extracted};
use super::obscura::{FetchedMarkdown, Obscura};
use crate::web::profile::pick_profile;
use crate::web::search::{apply_navigation_headers, ChainPosition};

/// Which engine produced the markdown. Recorded on
/// [`super::evidence::Evidence::fetch_path`] for debugging; not part of the
/// public tool response (#31).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchPath {
    Static,
    Browser,
}

/// Static HTTP request timeout, and the value `FetchError::Timeout` reports
/// on the static path. A static attempt that times out plus the Obscura
/// fallback ([`super::obscura::DEFAULT_TIMEOUT`]) must end inside the agent
/// loop's per-tool timeout (#100).
pub const STATIC_TIMEOUT: Duration = Duration::from_secs(10);

/// Tags whose text is never content (`head` covers `title`/`meta`/`style`).
const SKIP_TAGS: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "iframe", "nav", "footer", "form",
];

/// Lowercase markers of anti-bot interstitials, checked against a challenge
/// page served with HTTP 200 and against the body of a 403/429/503.
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
    /// Which addresses the static client and the Obscura check may reach.
    egress: EgressPolicy,
}

/// Why the static path handed over to Obscura; kept for the error message.
enum Handover {
    /// 403/429/503 whose body or `cf-mitigated` header marked it a challenge.
    ChallengeStatus(u16),
    Challenge,
    ThinMarkdown(usize),
    ContentType(String),
}

impl std::fmt::Display for Handover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChallengeStatus(code) => write!(f, "HTTP {code}"),
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
    /// JS-shell threshold, `obscura` on PATH with a 60 s timeout, egress
    /// policy from `FETCH_ALLOW_PRIVATE`.
    pub fn new() -> Self {
        Self::with_obscura(Obscura::default())
    }

    /// Same defaults with an injected Obscura handle (tests use the fixture double).
    pub fn with_obscura(obscura: Obscura) -> Self {
        Self::with_policy(obscura, EgressPolicy::from_env())
    }

    /// Same defaults with an explicit egress policy, applied to the static
    /// client and to `obscura`. Tests against a local `127.0.0.1` server
    /// pass [`EgressPolicy::AllowPrivate`] here instead of setting the env.
    pub fn with_policy(obscura: Obscura, egress: EgressPolicy) -> Self {
        Self::build(obscura, egress, reqwest::Client::builder())
    }

    /// Public-only fetcher whose client maps `host` to `addr` (a local
    /// stub), so a test can serve a "public" name from `127.0.0.1` and prove
    /// the redirect hop and resolver checks still apply past it. `reqwest`'s
    /// overrides bypass the resolver for `host` only.
    #[cfg(test)]
    pub(crate) fn public_only_with_dns_override(
        obscura: Obscura,
        host: &str,
        addr: std::net::SocketAddr,
    ) -> Self {
        Self::build(
            obscura,
            EgressPolicy::PublicOnly,
            reqwest::Client::builder().resolve(host, addr),
        )
    }

    fn build(obscura: Obscura, egress: EgressPolicy, builder: reqwest::ClientBuilder) -> Self {
        let mut builder = builder
            .timeout(STATIC_TIMEOUT)
            .redirect(redirect_policy(egress));
        if egress == EgressPolicy::PublicOnly {
            builder = builder.dns_resolver(Arc::new(EgressResolver));
        }
        let client = builder
            .build()
            .expect("static reqwest client config is valid");
        Self {
            client,
            obscura: obscura.with_egress(egress),
            max_body_bytes: super::MAX_BODY_BYTES,
            min_markdown_chars: 200,
            egress,
        }
    }

    /// Fetch `raw_url` as markdown. Tries the static path; falls back to
    /// Obscura on the triggers listed in the module docs.
    pub async fn fetch(&self, raw_url: &str) -> Result<(FetchedMarkdown, FetchPath), FetchError> {
        let url = normalize_url(raw_url, self.egress)?;
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
        let profile = pick_profile();
        let builder =
            apply_navigation_headers(self.client.get(url), &profile, ChainPosition::First, None);
        let mut response = builder.send().await.map_err(|err| {
            if let Some(denied) = denied_in_chain(&err) {
                // The redirect policy records the refused hop; a resolver
                // refusal reports the URL `reqwest` was connecting to.
                FetchError::Egress {
                    url: denied.hop.clone().unwrap_or_else(|| {
                        err.url()
                            .map_or_else(|| url.to_owned(), |target| target.to_string())
                    }),
                    reason: denied.reason.clone(),
                }
            } else if err.is_timeout() {
                FetchError::Timeout {
                    url: url.to_owned(),
                    after: STATIC_TIMEOUT,
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
            // Cloudflare sometimes signals a challenge on a body with no
            // textual marker (e.g. a JSON/API response); the header is
            // authoritative when present, so it is checked before reading
            // the (possibly large) body at all.
            let cf_challenge = response
                .headers()
                .get("cf-mitigated")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.eq_ignore_ascii_case("challenge"));
            if cf_challenge {
                return Ok(StaticOutcome::Fallback(Handover::ChallengeStatus(status)));
            }
            let body = self.read_capped_body(url, &mut response).await?;
            let text = String::from_utf8_lossy(&body);
            if is_challenge(&text) {
                return Ok(StaticOutcome::Fallback(Handover::ChallengeStatus(status)));
            }
            return Err(FetchError::Http {
                url: url.to_owned(),
                detail: format!("HTTP {status}"),
            });
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

        let body = self.read_capped_body(url, &mut response).await?;
        let text = String::from_utf8_lossy(&body);

        let markdown = if content_type.contains("html") {
            if is_challenge(&text) {
                return Ok(StaticOutcome::Fallback(Handover::Challenge));
            }
            main_content_markdown(&text, self.min_markdown_chars)
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
            markdown: super::clean::clean_markdown(&markdown),
        }))
    }

    /// Read the full body via repeated `.chunk()` calls, aborting as soon as
    /// the running total crosses `max_body_bytes`. Unlike `.bytes()`, this
    /// never fully buffers a chunked response (no `Content-Length`) before
    /// the cap applies.
    async fn read_capped_body(
        &self,
        url: &str,
        response: &mut reqwest::Response,
    ) -> Result<Vec<u8>, FetchError> {
        if response
            .content_length()
            .is_some_and(|len| len as usize > self.max_body_bytes)
        {
            return Err(self.too_large(url));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|err| FetchError::Http {
            url: url.to_owned(),
            detail: err.to_string(),
        })? {
            body.extend_from_slice(&chunk);
            if body.len() > self.max_body_bytes {
                return Err(self.too_large(url));
            }
        }
        Ok(body)
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
pub(super) fn html_to_markdown(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIP_TAGS.to_vec())
        .build()
        .convert(html)
        .map(|md| md.trim().to_owned())
        .unwrap_or_default()
}

/// The page's markdown: the main-content extraction when the #108 gate
/// keeps it, else the whole-page baseline.
fn main_content_markdown(html: &str, min_markdown_chars: usize) -> String {
    let page = main_content(html, min_markdown_chars);
    match page.extracted {
        Some(extracted) if page.kept => extracted,
        _ => page.baseline,
    }
}

/// Both conversions of one HTML page and the gate's verdict; the fixture
/// measurement test reads all three.
pub(super) struct MainContent {
    pub(super) baseline: String,
    pub(super) extracted: Option<String>,
    pub(super) kept: bool,
}

pub(super) fn main_content(html: &str, min_markdown_chars: usize) -> MainContent {
    let baseline = html_to_markdown(html);
    let extraction = extract(html, SKIP_TAGS);
    let extracted = extraction
        .as_ref()
        .map(|extraction| html_to_markdown(&extraction.html));
    let kept = keep_extracted(
        &baseline,
        extraction.as_ref().zip(extracted.as_deref()),
        min_markdown_chars,
    );
    MainContent {
        baseline,
        extracted,
        kept,
    }
}

#[cfg(test)]
mod tests;
