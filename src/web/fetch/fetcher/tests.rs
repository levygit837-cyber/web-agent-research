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

mod egress {
    use super::*;
    use crate::web::search::test_support::{StubReply, StubServer};

    fn no_browser() -> Obscura {
        Obscura::new("/nonexistent/obscura".into(), Duration::from_secs(1))
    }

    fn page() -> String {
        format!(
            "<html><body><h1>Page</h1><p>{}</p></body></html>",
            "words ".repeat(80)
        )
    }

    /// Routes: `/to-literal` and `/to-localhost` redirect to loopback
    /// (IP literal, then a name resolving to loopback); `/hop` redirects
    /// on the same host; anything else is a page.
    async fn stub() -> StubServer {
        StubServer::serve(|path: &str, _: &str| match path {
            "/to-literal" => StubReply::redirect(302, "http://127.0.0.1:1/secret"),
            "/to-localhost" => StubReply::redirect(302, "http://localhost:1/secret"),
            "/hop" => StubReply::redirect(302, "/landing"),
            _ => StubReply::text(200, &page()),
        })
        .await
    }

    /// A fetcher that sees the stub as the public name `public.test`.
    fn public_view(stub: &StubServer) -> (Fetcher, String) {
        let base = stub.base();
        let addr: std::net::SocketAddr = base
            .trim_start_matches("http://")
            .parse()
            .expect("stub addr");
        let fetcher = Fetcher::public_only_with_dns_override(no_browser(), "public.test", addr);
        (fetcher, format!("http://public.test:{}", addr.port()))
    }

    #[tokio::test]
    async fn redirect_to_loopback_literal_is_refused_with_typed_error() {
        let stub = stub().await;
        let (fetcher, base) = public_view(&stub);
        let err = fetcher
            .fetch(&format!("{base}/to-literal"))
            .await
            .unwrap_err();
        let FetchError::Egress { url, reason } = &err else {
            panic!("expected Egress, got {err:?}");
        };
        assert_eq!(url, "http://127.0.0.1:1/secret");
        assert!(reason.contains("127.0.0.1"), "{reason}");
        assert!(err
            .to_string()
            .starts_with("refused private-network target"));
    }

    #[tokio::test]
    async fn redirect_to_name_resolving_to_loopback_is_refused() {
        let stub = stub().await;
        let (fetcher, base) = public_view(&stub);
        let err = fetcher
            .fetch(&format!("{base}/to-localhost"))
            .await
            .unwrap_err();
        let FetchError::Egress { reason, .. } = &err else {
            panic!("expected Egress, got {err:?}");
        };
        assert!(reason.contains("localhost resolves to"), "{reason}");
    }

    #[tokio::test]
    async fn public_view_still_fetches_and_follows_safe_redirects() {
        let stub = stub().await;
        let (fetcher, base) = public_view(&stub);
        let (page, path) = fetcher.fetch(&format!("{base}/hop")).await.unwrap();
        assert_eq!(path, FetchPath::Static);
        assert_eq!(page.url, format!("{base}/landing"));
    }

    #[tokio::test]
    async fn loopback_literal_is_refused_before_any_request() {
        let stub = stub().await;
        let fetcher = Fetcher::with_policy(no_browser(), EgressPolicy::PublicOnly);
        let err = fetcher
            .fetch(&format!("{}/page", stub.base()))
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Egress { .. }), "{err:?}");
        assert_eq!(stub.hits("/page"), 0, "no request may reach the target");
    }

    #[tokio::test]
    async fn localhost_name_is_refused_at_resolve_time() {
        let stub = stub().await;
        let port = stub.base().rsplit(':').next().unwrap().to_owned();
        let fetcher = Fetcher::with_policy(no_browser(), EgressPolicy::PublicOnly);
        let err = fetcher
            .fetch(&format!("http://localhost:{port}/page"))
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Egress { .. }), "{err:?}");
        assert_eq!(stub.hits("/page"), 0);
    }

    #[tokio::test]
    async fn allow_private_restores_loopback_and_redirects() {
        let stub = stub().await;
        let fetcher = Fetcher::with_policy(no_browser(), EgressPolicy::AllowPrivate);
        let (page, _) = fetcher
            .fetch(&format!("{}/hop", stub.base()))
            .await
            .unwrap();
        assert_eq!(page.url, format!("{}/landing", stub.base()));
    }

    #[tokio::test]
    async fn obscura_fallback_is_checked_before_spawning() {
        // The fixture double would succeed; the check must stop it first.
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/obscura-fake.sh");
        let engine =
            Obscura::new(fixture, Duration::from_secs(5)).with_egress(EgressPolicy::PublicOnly);
        for url in ["http://127.0.0.1:9/page", "http://localhost:9/page"] {
            let err = engine.fetch_markdown(url).await.unwrap_err();
            assert!(matches!(err, FetchError::Egress { .. }), "{url}: {err:?}");
        }
        let allowed = Obscura::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/obscura-fake.sh"),
            Duration::from_secs(5),
        )
        .with_egress(EgressPolicy::AllowPrivate);
        assert!(allowed
            .fetch_markdown("http://127.0.0.1:9/page")
            .await
            .is_ok());
    }
}
