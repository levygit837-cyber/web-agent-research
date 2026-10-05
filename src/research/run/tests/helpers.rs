//! Shared fixtures for `run::tests`' themed test files: a canned gateway,
//! the env pointing `run_research_with` at it, a per-test Session path, and
//! the local or walled `ToolRegistry` the hermetic runs search and fetch
//! through.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::research::agent_loop::ToolRegistry;
use crate::research::dto::ResearchRequest;
use crate::research::synthesis::SynthesisSize;
use crate::test_support::{CannedResponse, CannedServer};
use crate::web::fetch::Fetcher;
use crate::web::search::tool::Searcher;

pub(super) use crate::research::agent_loop::test_support::{text_body, tool_call, tools_body};

/// Serve each response in order over plain HTTP; return the gateway
/// `/v1`-style base URL `point_env_at` expects.
pub(super) fn spawn_server(responses: Vec<String>) -> String {
    let server = CannedServer::spawn(
        responses
            .into_iter()
            .map(|body| CannedResponse::text(200, body))
            .collect(),
    );
    format!("{}/v1", server.base())
}

/// Like [`spawn_server`], but returns the server itself so wire-body
/// assertions (e.g. `prompt_cache_key`) can inspect exactly what
/// `run_research_with` sent via `.requests()`.
pub(super) fn spawn_capturing_server(responses: Vec<String>) -> (String, CannedServer) {
    let server = CannedServer::spawn(
        responses
            .into_iter()
            .map(|body| CannedResponse::text(200, body))
            .collect(),
    );
    let base = format!("{}/v1", server.base());
    (base, server)
}

pub(super) fn wire_body_of(request: &str) -> serde_json::Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request must carry a body");
    serde_json::from_str(body).expect("wire body must be JSON")
}

pub(super) fn temp_session_out(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "web-agent-research-{tag}-{}-{nanos}.jsonl",
        std::process::id()
    ))
}

pub(super) fn hermetic_request(session_tag: &str) -> ResearchRequest {
    ResearchRequest {
        goal: "What is the Obscura headless browser?".to_owned(),
        size: SynthesisSize::Small,
        max_turns: 8,
        max_turns_cap: 25,
        deadline_secs: 300,
        session_id: Some(format!("test-{session_tag}")),
        session_out: Some(temp_session_out(session_tag)),
    }
}

pub(super) fn point_env_at(base_url: &str) {
    std::env::set_var("GATEWAY_BASE_URL", base_url);
    std::env::set_var("GATEWAY_API_KEY", "dummy");
    std::env::remove_var("GATEWAY_MODEL");
}

/// Local web: a page server plus a search stub whose Hits point at it.
pub(super) async fn local_web() -> (ToolRegistry, String) {
    use crate::web::search::test_support::{
        ddg_rows, sp_home_form, sp_rows, FanoutStub, StubReply, StubServer,
    };
    let pages = StubServer::serve(|_: &str, _: &str| {
        StubReply::text(
            200,
            &format!(
                "<html><body><h1>Obscura</h1><p>{}</p></body></html>",
                "Obscura is a headless browser written in Rust. ".repeat(8)
            ),
        )
    })
    .await;
    let page_url = format!("{}/obscura", pages.base());
    let (search_base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        ddg_rows(&page_url, "Obscura", "headless browser"),
        sp_home_form(),
        sp_rows(&page_url, "Obscura", "headless browser"),
        false,
    ))
    .await;
    let searcher = Searcher::with_bases(
        &format!("{search_base}/html/"),
        &format!("{search_base}/"),
        &format!("{search_base}/sp/search"),
        &format!("{search_base}/search"),
        &format!("{search_base}/search"),
        &format!("{search_base}/search"),
    );
    let fetcher = Fetcher::with_policy(
        crate::web::fetch::Obscura::new(
            "/nonexistent/obscura".into(),
            std::time::Duration::from_secs(1),
        ),
        crate::web::fetch::EgressPolicy::AllowPrivate,
    );
    (ToolRegistry::new(searcher, fetcher), page_url)
}

/// Serves one long page (1,000 numbered paragraphs, about 60K chars of
/// markdown, 3 parts at the default part size) and a registry whose
/// search is unreachable: the model is scripted to fetch directly.
pub(super) async fn long_page_web() -> (
    ToolRegistry,
    String,
    crate::web::search::test_support::StubServer,
) {
    use crate::web::search::test_support::{StubReply, StubServer};
    let pages = StubServer::serve(|_: &str, _: &str| {
        let paragraphs: String = (0..1_000)
            .map(|i| format!("<p>Paragraph {i:04}: filler words to make the page long enough.</p>"))
            .collect();
        StubReply::text(
            200,
            &format!("<html><body><h1>Long</h1>{paragraphs}</body></html>"),
        )
    })
    .await;
    let page_url = format!("{}/long", pages.base());
    let dead = "http://127.0.0.1:9/";
    let registry = ToolRegistry::new(
        Searcher::with_bases(dead, dead, dead, dead, dead, dead),
        Fetcher::with_policy(
            crate::web::fetch::Obscura::new(
                "/nonexistent/obscura".into(),
                std::time::Duration::from_secs(1),
            ),
            crate::web::fetch::EgressPolicy::AllowPrivate,
        ),
    );
    (registry, page_url, pages)
}

/// A search stub where every leg on every provider is bot-wall
/// Challenge-walled (#53): `searcher.search` always returns
/// `SearchProviderError::AllFailed { all_challenged: true, .. }`. No
/// page server: a walled search never reaches fetch. #66: `Searcher`'s
/// hermetic `Governor` always enables all five providers with no
/// `SEARCH_ENGINES` override available through this constructor, so
/// Brave/Yahoo/Bing each get their own tiny stub tuned to their real
/// Challenge marker (Brave/Bing: HTTP 429; Yahoo: a redirect to a
/// `guce.yahoo.com` host) rather than sharing the DDG/Startpage stub,
/// whose unrecognized routes would 404 into a plain `Upstream` and
/// break the "every enabled engine was Challenge-walled" acceptance.
pub(super) async fn walled_web() -> ToolRegistry {
    use crate::web::search::test_support::{sp_home_form, FanoutStub, StubReply, StubServer};
    let (search_base, _hits) = StubServer::serve_routes(FanoutStub::single_page(
        r#"<div id="anomaly-modal"></div>"#.to_string(),
        sp_home_form(),
        r#"<script id="anubis_challenge" type="application/json">{}</script>"#.to_string(),
        false,
    ))
    .await;
    let brave_stub =
        StubServer::serve(|_: &str, _: &str| StubReply::text(429, "rate limited")).await;
    let yahoo_stub = StubServer::serve(|_: &str, _: &str| {
        StubReply::redirect(302, "https://guce.yahoo.com/consent")
    })
    .await;
    let bing_stub = StubServer::serve(|_: &str, _: &str| StubReply::text(429, "blocked")).await;
    let searcher = Searcher::with_bases(
        &format!("{search_base}/html/"),
        &format!("{search_base}/"),
        &format!("{search_base}/sp/search"),
        &brave_stub.base(),
        &yahoo_stub.base(),
        &bing_stub.base(),
    );
    let fetcher = Fetcher::with_policy(
        crate::web::fetch::Obscura::new(
            "/nonexistent/obscura".into(),
            std::time::Duration::from_secs(1),
        ),
        crate::web::fetch::EgressPolicy::AllowPrivate,
    );
    ToolRegistry::new(searcher, fetcher)
}
