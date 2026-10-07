//! Fetch rewrite for `https://crates.io/crates/<name>[/...]` (#109).
//!
//! The crate page is client-rendered (`ssr = false`): a static fetch gets
//! an empty shell and would start Obscura. Instead the page is served from
//! the sparse index (versions, yanked flags, `rust_version`; no rate
//! limit) plus one crates.io API call (description, repository, homepage,
//! default version), which passes the process-wide 1 request/s gate
//! ([`crate::web::crates_io::API_GATE`]). Both requests use the client of
//! the [`Fetcher`] (same egress policy and timeout) and the honest API
//! User-Agent. The resulting Evidence keeps the human crates.io URL.
//!
//! The index is required: a 404 there means the crate does not exist. The
//! API only enriches the block; when it fails, the block says so and is
//! still served from the index.

use serde::Deserialize;

use super::error::FetchError;
use super::fetcher::{send_error, Fetcher};
use super::obscura::FetchedMarkdown;
use crate::web::crates_io::{docs_url, index_path, is_crate_name, API_BASE, API_GATE, INDEX_BASE};
use crate::web::search::apply_api_headers;

/// Versions listed in the block, newest first; the rest are counted only.
const LISTED_VERSIONS: usize = 15;

/// Where the rewrite reads from. Production uses the real hosts; tests
/// point both at a loopback stub.
#[derive(Debug, Clone)]
pub(super) struct CratesIoBases {
    pub(super) api: String,
    pub(super) index: String,
}

impl Default for CratesIoBases {
    fn default() -> Self {
        Self {
            api: API_BASE.to_owned(),
            index: INDEX_BASE.to_owned(),
        }
    }
}

/// A crates.io crate page this rewrite serves: the crate name and the
/// version segment, when the URL names one (`/crates/bytes/1.6.0`).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CratePage {
    pub(super) name: String,
    pub(super) version: Option<String>,
}

/// `Some` for `http(s)://crates.io/crates/<valid name>[/...]`, any query
/// or fragment; `None` for every other URL (search, users, docs.rs, ...).
pub(super) fn crate_page(url: &str) -> Option<CratePage> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if !(host.eq_ignore_ascii_case("crates.io") || host.eq_ignore_ascii_case("www.crates.io")) {
        return None;
    }
    let mut segments = parsed.path_segments()?.filter(|s| !s.is_empty());
    if segments.next()? != "crates" {
        return None;
    }
    let name = segments.next().filter(|name| is_crate_name(name))?;
    let version = segments
        .next()
        .filter(|segment| segment.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_owned);
    Some(CratePage {
        name: name.to_owned(),
        version,
    })
}

/// One sparse-index line, reduced to what the block shows.
#[derive(Debug, Deserialize)]
struct IndexVersion {
    vers: String,
    #[serde(default)]
    yanked: bool,
    #[serde(default)]
    rust_version: Option<String>,
    #[serde(default)]
    pubtime: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiBody {
    #[serde(rename = "crate")]
    krate: ApiCrate,
}

/// The API fields the index does not carry.
#[derive(Debug, Default, Deserialize)]
struct ApiCrate {
    #[serde(default)]
    default_version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    documentation: Option<String>,
}

impl Fetcher {
    /// Serve a crates.io crate page as markdown (module docs). `url` is the
    /// normalized human URL and becomes [`FetchedMarkdown::url`].
    pub(super) async fn fetch_crate_page(
        &self,
        url: &str,
        page: &CratePage,
    ) -> Result<FetchedMarkdown, FetchError> {
        let bases = &self.crates_io;
        let index_url = format!("{}/{}", bases.index, index_path(&page.name));
        let index_body = self.get_api_text(&index_url, "text/plain").await?;
        let versions = match index_body {
            Ok(body) => parse_index(&index_url, &body)?,
            Err(404) => {
                return Err(FetchError::Http {
                    url: url.to_owned(),
                    detail: format!(
                        "crate `{}` not found in the crates.io index (HTTP 404)",
                        page.name
                    ),
                })
            }
            Err(status) => {
                return Err(FetchError::Http {
                    url: index_url,
                    detail: format!("HTTP {status}"),
                })
            }
        };

        let api_url = format!("{}/crates/{}?include=", bases.api, page.name);
        API_GATE.wait().await;
        let api = match self.get_api_text(&api_url, "application/json").await {
            Ok(Ok(body)) => serde_json::from_str::<ApiBody>(&body)
                .map(|body| body.krate)
                .map_err(|err| format!("unexpected crates.io API body: {err}")),
            Ok(Err(status)) => Err(format!("crates.io API returned HTTP {status}")),
            Err(err) => Err(err.to_string()),
        };
        Ok(FetchedMarkdown {
            url: url.to_owned(),
            markdown: render(page, &versions, api),
        })
    }

    /// GET `url` with the API headers: `Ok(Ok(body))` on 2xx,
    /// `Ok(Err(status))` on any other status, `Err` on a transport failure
    /// or an oversize body.
    async fn get_api_text(
        &self,
        url: &str,
        accept: &str,
    ) -> Result<Result<String, u16>, FetchError> {
        let mut response = apply_api_headers(self.client.get(url), accept)
            .send()
            .await
            .map_err(|err| send_error(url, &err))?;
        let status = response.status();
        if !status.is_success() {
            return Ok(Err(status.as_u16()));
        }
        let body = self.read_capped_body(url, &mut response).await?;
        Ok(Ok(String::from_utf8_lossy(&body).into_owned()))
    }
}

/// Index file -> versions in publish order. A line that does not parse is
/// skipped; a file with no parseable line is a typed error.
fn parse_index(index_url: &str, body: &str) -> Result<Vec<IndexVersion>, FetchError> {
    let versions: Vec<IndexVersion> = body
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if versions.is_empty() {
        return Err(FetchError::Http {
            url: index_url.to_owned(),
            detail: "crates.io index file has no parseable version line".to_owned(),
        });
    }
    Ok(versions)
}

/// The markdown block: header, links, version summary, newest versions.
fn render(page: &CratePage, versions: &[IndexVersion], api: Result<ApiCrate, String>) -> String {
    let (api, api_error) = match api {
        Ok(api) => (api, None),
        Err(err) => (ApiCrate::default(), Some(err)),
    };
    let latest = api
        .default_version
        .as_deref()
        .or_else(|| {
            versions
                .iter()
                .rev()
                .find(|v| !v.yanked)
                .map(|v| v.vers.as_str())
        })
        .unwrap_or(versions[versions.len() - 1].vers.as_str());
    let mut out = format!("# {} {latest}\n\n", page.name);
    if let Some(description) = api
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        out.push_str(description);
        out.push_str("\n\n");
    }
    out.push_str(&format!("- Docs: {}\n", docs_url(&page.name)));
    for (label, link) in [
        ("Repository", &api.repository),
        ("Homepage", &api.homepage),
        ("Documentation", &api.documentation),
    ] {
        if let Some(link) = link.as_deref().filter(|l| !l.trim().is_empty()) {
            out.push_str(&format!("- {label}: {}\n", link.trim()));
        }
    }
    if let Some(entry) = versions.iter().find(|v| v.vers == latest) {
        out.push_str(&format!(
            "- Latest version: {latest} (rust_version {})\n",
            entry.rust_version.as_deref().unwrap_or("not declared")
        ));
    }
    let yanked = versions.iter().filter(|v| v.yanked).count();
    out.push_str(&format!(
        "- Versions: {} published, {yanked} yanked\n",
        versions.len()
    ));
    if let Some(wanted) = &page.version {
        match versions.iter().find(|v| &v.vers == wanted) {
            Some(v) => out.push_str(&format!(
                "- Requested version: {wanted} ({}, rust_version {})\n",
                if v.yanked { "yanked" } else { "not yanked" },
                v.rust_version.as_deref().unwrap_or("not declared")
            )),
            None => out.push_str(&format!(
                "- Requested version: {wanted} (not in the index)\n"
            )),
        }
    }
    if let Some(err) = api_error {
        out.push_str(&format!(
            "- Note: description and links unavailable ({err}); versions are from the index\n"
        ));
    }
    let shown = versions.len().min(LISTED_VERSIONS);
    out.push_str(&format!(
        "\n## Versions (newest first, {shown} of {})\n\n| Version | Published | Yanked | rust_version |\n|---|---|---|---|\n",
        versions.len()
    ));
    for v in versions.iter().rev().take(LISTED_VERSIONS) {
        let published = v
            .pubtime
            .as_deref()
            .map_or("", |t| t.get(..10).unwrap_or(t));
        out.push_str(&format!(
            "| {} | {published} | {} | {} |\n",
            v.vers,
            if v.yanked { "yes" } else { "no" },
            v.rust_version.as_deref().unwrap_or("")
        ));
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_support::{CannedResponse, CannedServer};
    use crate::web::fetch::{EgressPolicy, FetchPath, Obscura};

    const INDEX_BYTES: &str = include_str!("../../../tests/fixtures/cratesio/index/by/te/bytes");
    const CRATE_BYTES: &str = include_str!("../../../tests/fixtures/cratesio/crate-bytes.json");
    const CRATE_MISSING: &str = include_str!("../../../tests/fixtures/cratesio/crate-missing.json");

    /// A loopback fetcher whose index and API both point at `server`. The
    /// browser binary does not exist, so any Obscura start would fail the
    /// test with `FallbackUnavailable`.
    fn fetcher(server: &CannedServer) -> Fetcher {
        Fetcher::with_policy(
            Obscura::new("/nonexistent/obscura".into(), Duration::from_secs(1)),
            EgressPolicy::AllowPrivate,
        )
        .with_crates_io_bases(
            format!("{}/api/v1", server.base()),
            server.base().to_owned(),
        )
    }

    fn request_line(raw: &str) -> &str {
        raw.lines().next().unwrap_or("")
    }

    #[test]
    fn crate_page_urls() {
        let page = |name: &str, version: Option<&str>| {
            Some(CratePage {
                name: name.to_owned(),
                version: version.map(str::to_owned),
            })
        };
        let cases = [
            ("https://crates.io/crates/bytes", page("bytes", None)),
            ("https://crates.io/crates/bytes/", page("bytes", None)),
            (
                "https://crates.io/crates/bytes/1.6.0",
                page("bytes", Some("1.6.0")),
            ),
            (
                "https://crates.io/crates/serde_json/versions",
                page("serde_json", None),
            ),
            (
                "http://www.crates.io/crates/tokio-util?tab=readme",
                page("tokio-util", None),
            ),
            ("https://crates.io/crates/", None),
            ("https://crates.io/search?q=bytes", None),
            ("https://crates.io/crates/1bad", None),
            ("https://docs.rs/bytes", None),
            ("https://example.org/crates/bytes", None),
        ];
        for (url, want) in cases {
            assert_eq!(crate_page(url), want, "{url}");
        }
    }

    #[tokio::test]
    async fn crate_page_is_served_from_index_and_api() {
        let server = CannedServer::spawn(vec![
            CannedResponse::text(200, INDEX_BYTES),
            CannedResponse::text(200, CRATE_BYTES),
        ]);
        let (page, path) = fetcher(&server)
            .fetch("https://crates.io/crates/bytes")
            .await
            .expect("rewrite serves the page");
        assert_eq!(path, FetchPath::Api);
        assert_eq!(page.url, "https://crates.io/crates/bytes");
        let md = &page.markdown;
        for want in [
            "# bytes 1.12.1\n\nTypes and traits for working with bytes",
            "- Docs: https://docs.rs/bytes",
            "- Repository: https://github.com/tokio-rs/bytes",
            "- Latest version: 1.12.1 (rust_version 1.57)",
            "- Versions: 21 published, 4 yanked",
            "## Versions (newest first, 15 of 21)",
            "| 1.12.1 | 2026-07-08 | no | 1.57 |",
            "| 1.6.0 | 2024-03-22 | yes | 1.39 |",
            "| 0.5.3 | 2019-12-12 | no |  |",
        ] {
            assert!(md.contains(want), "missing {want:?} in:\n{md}");
        }
        assert!(!md.contains("| 0.5.2 |"), "only 15 versions are listed");
        let requests = server.requests();
        assert_eq!(request_line(&requests[0]), "GET /by/te/bytes HTTP/1.1");
        assert_eq!(
            request_line(&requests[1]),
            "GET /api/v1/crates/bytes?include= HTTP/1.1"
        );
        for raw in &requests {
            let ua = raw
                .lines()
                .find_map(|l| l.strip_prefix("user-agent: "))
                .expect("user-agent sent");
            assert!(ua.starts_with("web-agent-research/"), "{ua}");
            assert!(ua.contains("(+"), "contact in {ua}");
        }
    }

    #[tokio::test]
    async fn requested_version_is_reported() {
        let server = CannedServer::spawn(vec![
            CannedResponse::text(200, INDEX_BYTES),
            CannedResponse::text(200, CRATE_BYTES),
        ]);
        let (page, _) = fetcher(&server)
            .fetch("https://crates.io/crates/bytes/1.6.0")
            .await
            .unwrap();
        assert!(page
            .markdown
            .contains("- Requested version: 1.6.0 (yanked, rust_version 1.39)"));
        assert_eq!(page.url, "https://crates.io/crates/bytes/1.6.0");
    }

    #[tokio::test]
    async fn api_failure_still_serves_the_index_block() {
        let server = CannedServer::spawn(vec![
            CannedResponse::text(200, INDEX_BYTES),
            CannedResponse::text(503, "{}"),
        ]);
        let (page, _) = fetcher(&server)
            .fetch("https://crates.io/crates/bytes")
            .await
            .unwrap();
        let md = &page.markdown;
        assert!(
            md.starts_with("# bytes 1.12.1\n\n- Docs: https://docs.rs/bytes"),
            "{md}"
        );
        assert!(md.contains(
            "- Note: description and links unavailable (crates.io API returned HTTP 503)"
        ));
        assert!(!md.contains("Repository"));
    }

    #[tokio::test]
    async fn missing_crate_is_a_typed_404_without_an_api_call() {
        let server = CannedServer::spawn(vec![
            CannedResponse::text(404, "<Error>NoSuchKey</Error>"),
            CannedResponse::text(404, CRATE_MISSING),
        ]);
        let err = fetcher(&server)
            .fetch("https://crates.io/crates/zzqxjv-no-such-crate-qq")
            .await
            .unwrap_err();
        let FetchError::Http { url, detail } = &err else {
            panic!("expected Http, got {err:?}");
        };
        assert_eq!(url, "https://crates.io/crates/zzqxjv-no-such-crate-qq");
        assert!(
            detail.contains("not found in the crates.io index"),
            "{detail}"
        );
        assert_eq!(server.hits(), 1);
    }

    #[tokio::test]
    async fn unparseable_index_is_a_typed_error() {
        let server = CannedServer::spawn(vec![CannedResponse::text(200, "<html></html>")]);
        let err = fetcher(&server)
            .fetch("https://crates.io/crates/bytes")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, FetchError::Http { detail, .. } if detail.contains("no parseable version line")),
            "{err:?}"
        );
    }
}
