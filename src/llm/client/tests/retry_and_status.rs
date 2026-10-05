//! Status-code mapping, timeout, retry/backoff, Retry-After, the run
//! deadline, and the pre-flight missing-key check.

use std::io::{Read, Write};
use std::time::{Duration, SystemTime};

use super::super::*;
use super::helpers::{messages, ok, single_attempt_gateway, spawn_server};
use crate::test_support::CannedResponse;

type StatusCheck = fn(&GatewayError) -> bool;

/// Every status class lands on the right variant and the right side of
/// `is_retryable`, and Client/Server errors carry the upstream message.
#[tokio::test]
async fn status_mapping() {
    let detail_body = r#"{"error":{"message":"model not found: m"}}"#;
    let cases: Vec<(CannedResponse, StatusCheck)> = vec![
        (CannedResponse::status(401), |e| {
            matches!(e, GatewayError::Auth) && !e.is_retryable()
        }),
        (CannedResponse::status(403), |e| {
            matches!(e, GatewayError::Forbidden(_)) && !e.is_retryable()
        }),
        (
            CannedResponse::status(429).with_header("Retry-After", "7"),
            |e| {
                matches!(
                    e,
                    GatewayError::RateLimited {
                        retry_after_secs: Some(7)
                    }
                ) && e.is_retryable()
            },
        ),
        (CannedResponse::text(500, "upstream exploded"), |e| {
            matches!(e, GatewayError::Server { status: 500, detail, .. } if detail == "upstream exploded")
                && e.is_retryable()
        }),
        (CannedResponse::status(502), |e| {
            matches!(e, GatewayError::Server { status: 502, .. }) && e.is_retryable()
        }),
        (
            CannedResponse::status(503).with_header("Retry-After", "4"),
            |e| {
                matches!(
                    e,
                    GatewayError::Server {
                        status: 503,
                        retry_after_secs: Some(4),
                        ..
                    }
                ) && e.is_retryable()
            },
        ),
        (
            CannedResponse::status(529).with_header("Retry-After", "2"),
            |e| {
                matches!(
                    e,
                    GatewayError::Server {
                        status: 529,
                        retry_after_secs: Some(2),
                        ..
                    }
                ) && e.is_retryable()
            },
        ),
        (
            CannedResponse::status(500).with_header("Retry-After", "9"),
            |e| {
                matches!(
                    e,
                    GatewayError::Server {
                        status: 500,
                        retry_after_secs: None,
                        ..
                    }
                )
            },
        ),
        (CannedResponse::status(501), |e| {
            matches!(e, GatewayError::Server { status: 501, .. }) && !e.is_retryable()
        }),
        (CannedResponse::status(505), |e| {
            matches!(e, GatewayError::Server { status: 505, .. }) && !e.is_retryable()
        }),
        (CannedResponse::text(404, detail_body), |e| {
            matches!(e, GatewayError::Client { status: 404, detail } if detail == "model not found: m")
                && !e.is_retryable()
        }),
        (CannedResponse::status(400), |e| {
            matches!(e, GatewayError::Client { status: 400, .. }) && !e.is_retryable()
        }),
        (CannedResponse::status(422), |e| {
            matches!(e, GatewayError::Client { status: 422, .. }) && !e.is_retryable()
        }),
        (CannedResponse::text(200, "not json"), |e| {
            matches!(e, GatewayError::Parse(_)) && !e.is_retryable()
        }),
        (CannedResponse::text(200, r#"{"choices": []}"#), |e| {
            matches!(e, GatewayError::EmptyChoices) && !e.is_retryable()
        }),
    ];
    for (canned, check) in cases {
        let status = canned.status;
        let (base_url, _) = spawn_server(vec![canned]);
        let err = single_attempt_gateway(base_url)
            .chat(&messages())
            .await
            .expect_err(&format!("HTTP {status} must be an error"));
        assert!(check(&err), "HTTP {status} mapped wrong: {err:?} ({err})");
    }
}

/// An upstream message is bounded so an echoing gateway cannot flood
/// stderr.
#[tokio::test]
async fn client_error_detail_is_bounded() {
    let (base_url, _) = spawn_server(vec![CannedResponse::text(400, "x".repeat(5_000))]);
    let err = single_attempt_gateway(base_url)
        .chat(&messages())
        .await
        .expect_err("400 must fail");
    let GatewayError::Client { detail, .. } = &err else {
        panic!("expected Client, got {err:?}");
    };
    assert!(detail.chars().count() <= MAX_DETAIL_CHARS + 1, "{detail}");
}

#[tokio::test]
async fn timeout_maps_and_is_retryable() {
    let (base_url, _) = spawn_server(vec![ok().with_delay(Duration::from_secs(5))]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned()).with_max_attempts(1),
        reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .expect("test client builds"),
    );
    let err = gateway
        .chat(&messages())
        .await
        .expect_err("slow server must time out");
    assert!(
        matches!(err, GatewayError::Timeout) && err.is_retryable(),
        "slow server must map to retryable Timeout, got {err:?} ({err})"
    );
}

/// A hung gateway costs at most two attempts, whatever `max_attempts`
/// allows. The listener accepts every connection and never answers, so
/// each attempt is counted the moment it connects.
#[tokio::test]
async fn timeout_is_retried_at_most_once() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener binds");
    let addr = listener.local_addr().expect("address");
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&accepted);
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            held.push(stream);
        }
    });
    let gateway = Gateway::with_client(
        GatewayConfig::new(
            format!("http://{addr}"),
            "test-key".to_owned(),
            "m".to_owned(),
        )
        .with_max_attempts(4),
        reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .expect("test client builds"),
    );
    let err = gateway.chat(&messages()).await.expect_err("must time out");
    assert!(matches!(err, GatewayError::Timeout), "{err:?}");
    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A 2xx whose body stalls mid-read is a retryable timeout, not a parse
/// error: the status arrived, the payload did not.
#[tokio::test]
async fn body_read_timeout_is_a_timeout_not_a_parse_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener binds");
    let addr = listener.local_addr().expect("address");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 500\r\n\r\n{\"choices\":",
            );
            std::thread::sleep(Duration::from_secs(3));
        }
    });
    let gateway = Gateway::with_client(
        GatewayConfig::new(format!("http://{addr}"), "k".to_owned(), "m".to_owned())
            .with_max_attempts(1),
        reqwest::Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .expect("test client builds"),
    );
    let err = gateway.chat(&messages()).await.expect_err("stalled body");
    assert!(
        err.is_retryable() && !matches!(err, GatewayError::Parse(_)),
        "{err:?}"
    );
}

/// The run deadline caps the attempt timeout: a client with no timeout of
/// its own still gives up when the run's time is up.
#[tokio::test]
async fn run_deadline_caps_the_attempt_timeout() {
    let (base_url, _) = spawn_server(vec![ok().with_delay(Duration::from_secs(5))]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "k".to_owned(), "m".to_owned()).with_max_attempts(3),
        reqwest::Client::new(),
    )
    .with_deadline(tokio::time::Instant::now() + Duration::from_millis(300));
    let started = std::time::Instant::now();
    let err = gateway.chat(&messages()).await.expect_err("deadline");
    assert!(matches!(err, GatewayError::Timeout), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// A retry whose backoff would end past the run deadline is not tried.
#[tokio::test]
async fn no_retry_past_the_run_deadline() {
    let (base_url, server) = spawn_server(vec![
        CannedResponse::status(503).with_header("Retry-After", "30"),
        ok(),
    ]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "k".to_owned(), "m".to_owned()).with_max_attempts(3),
        reqwest::Client::new(),
    )
    .with_deadline(tokio::time::Instant::now() + Duration::from_secs(5));
    let err = gateway.chat(&messages()).await.expect_err("503");
    assert!(
        matches!(err, GatewayError::Server { status: 503, .. }),
        "{err:?}"
    );
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn retry_then_success() {
    let (base_url, hits) = spawn_server(vec![CannedResponse::status(429), ok()]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned()).with_max_attempts(3),
        reqwest::Client::new(),
    );
    let reply = gateway
        .chat(&messages())
        .await
        .expect("second attempt must succeed");
    assert_eq!(reply.output, "gateway ok");
    assert_eq!(hits.hits(), 2);
}

/// Rejections are never retried: one hit each.
#[tokio::test]
async fn rejections_are_not_retried() {
    for canned in [
        CannedResponse::status(401),
        CannedResponse::status(400),
        CannedResponse::status(501),
        CannedResponse::text(200, "not json"),
    ] {
        let status = canned.status;
        let (base_url, hits) = spawn_server(vec![canned, ok()]);
        let gateway = Gateway::with_client(
            GatewayConfig::new(base_url, "test-key".to_owned(), "m".to_owned())
                .with_max_attempts(3),
            reqwest::Client::new(),
        );
        gateway.chat(&messages()).await.expect_err("must fail");
        // Give a would-be retry no chance to land: exactly one server hit.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(hits.hits(), 1, "HTTP {status} was retried");
    }
}

/// A base URL reqwest cannot build a request from fails once, as a
/// non-retryable error, without the URL in the message.
#[tokio::test]
async fn unbuildable_url_is_not_retried() {
    let gateway = Gateway::with_client(
        GatewayConfig::new(
            "localhost:8317/v1".to_owned(),
            "k".to_owned(),
            "m".to_owned(),
        )
        .with_max_attempts(3),
        reqwest::Client::new(),
    );
    let started = std::time::Instant::now();
    let err = gateway.chat(&messages()).await.expect_err("bad url");
    assert!(!err.is_retryable(), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "it was retried"
    );
}

/// A transport failure never prints the URL, which may carry credentials.
#[tokio::test]
async fn transport_error_omits_the_url() {
    let gateway = single_attempt_gateway("http://user:hunter2@127.0.0.1:9".to_owned());
    let err = gateway.chat(&messages()).await.expect_err("refused");
    assert!(matches!(err, GatewayError::Transport(_)), "{err:?}");
    let text = err.to_string();
    assert!(
        !text.contains("hunter2") && !text.contains("127.0.0.1:9"),
        "{text}"
    );
}

#[test]
fn retry_after_reads_seconds_and_http_dates() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let headers = |value: &str| {
        let mut map = reqwest::header::HeaderMap::new();
        map.insert(
            reqwest::header::RETRY_AFTER,
            value.parse().expect("header value"),
        );
        map
    };
    assert_eq!(parse_retry_after(&headers("12"), now), Some(12));
    let later = httpdate::fmt_http_date(now + Duration::from_secs(45));
    assert_eq!(parse_retry_after(&headers(&later), now), Some(45));
    let earlier = httpdate::fmt_http_date(now - Duration::from_secs(45));
    assert_eq!(parse_retry_after(&headers(&earlier), now), Some(0));
    assert_eq!(parse_retry_after(&headers("soon"), now), None);
    assert_eq!(
        parse_retry_after(&reqwest::header::HeaderMap::new(), now),
        None
    );
}

/// Full jitter: never above the exponential ceiling, and not a constant.
#[test]
fn backoff_shape() {
    assert_eq!(backoff_ceiling(1), Duration::from_secs(1));
    assert_eq!(backoff_ceiling(2), Duration::from_secs(2));
    assert_eq!(backoff_ceiling(9), Duration::from_secs(8));
    assert_eq!(backoff_delay(2, Some(30)), Duration::from_secs(30));
    assert_eq!(backoff_delay(1, Some(3600)), Duration::from_secs(60));
    let draws: Vec<Duration> = (0..64).map(|_| backoff_delay(3, None)).collect();
    assert!(draws.iter().all(|d| *d <= backoff_ceiling(3)), "{draws:?}");
    assert!(
        draws.iter().any(|d| *d != draws[0]),
        "jitter must vary: {draws:?}"
    );
}

#[tokio::test]
async fn missing_key_never_touches_io() {
    let gateway = Gateway::new(GatewayConfig::new(
        "http://127.0.0.1:1".to_owned(),
        String::new(),
        "m".to_owned(),
    ));
    let err = gateway
        .chat(&messages())
        .await
        .expect_err("empty key must fail pre-flight");
    assert!(matches!(err, GatewayError::MissingApiKey));
}

#[tokio::test]
async fn forbidden_surfaces_the_upstream_message_and_does_not_retry() {
    let (base_url, hits) = spawn_server(vec![
        CannedResponse::text(
            403,
            r#"{"type":"error","error":{"type":"FreeTierError","message":"free tier gated"}}"#,
        ),
        ok(),
    ]);
    let gateway = Gateway::with_client(
        GatewayConfig::new(base_url, "k".to_owned(), "m".to_owned()).with_max_attempts(3),
        reqwest::Client::new(),
    );
    let err = gateway.chat(&messages()).await.expect_err("403 must fail");
    assert!(
        matches!(&err, GatewayError::Forbidden(detail) if detail == "free tier gated"),
        "got {err:?}"
    );
    assert!(!err.to_string().contains("bad API key"), "{err}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hits.hits(), 1);
}
