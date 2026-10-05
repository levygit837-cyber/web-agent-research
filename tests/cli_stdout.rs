//! The real binary against a local canned gateway (#99, #100): with
//! `RUST_LOG=info`, stdout holds exactly one JSON object and the logs go
//! to stderr; a malformed `GATEWAY_BASE_URL` exits 2; a gateway that never
//! answers ends within `--deadline-secs`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const ANSWER: &str = "Summary of the finding.\n\n## Findings\n\n- Point one.\n";

/// Serve `responses` as `(status, body)` in order, one per connection.
fn canned_gateway(responses: Vec<(u16, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener binds");
    let addr = listener.local_addr().expect("address");
    std::thread::spawn(move || {
        for (status, body) in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut seen = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read headers, then the declared body, before answering.
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&seen);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if seen.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let reply = format!(
                "HTTP/1.1 {status} x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    format!("http://{addr}/v1")
}

fn text_body(text: &str) -> String {
    serde_json::json!({
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
    .to_string()
}

fn run(base_url: &str, tag: &str, args: &[&str]) -> Output {
    let home = std::env::temp_dir().join(format!("war-cli-stdout-{tag}-{}", std::process::id()));
    Command::new(env!("CARGO_BIN_EXE_web-agent-research"))
        .arg("research")
        .arg("What is the Obscura headless browser?")
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("RUST_LOG", "info")
        .env("GATEWAY_BASE_URL", base_url)
        .env("GATEWAY_API_KEY", "test-key")
        .env("GATEWAY_MODEL", "m")
        .env("WEB_AGENT_RESEARCH_HOME", &home)
        .output()
        .expect("binary runs")
}

#[test]
fn info_logs_go_to_stderr_and_stdout_is_one_json_object() {
    // The 500 forces a retry, which logs at INFO.
    let base = canned_gateway(vec![
        (500, r#"{"error":{"message":"boom"}}"#.to_owned()),
        (200, text_body(ANSWER)),
    ]);
    let output = run(
        &base,
        "json",
        &["--json", "--max-turns", "1", "--max-turns-cap", "1"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status {:?}; stderr: {stderr}",
        output.status
    );
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim_end()).expect("stdout is exactly one JSON object");
    assert!(value.is_object(), "{stdout}");
    assert_eq!(value["turns_used"], 1);
    assert_eq!(value["turn_budget"], 1);
    assert!(stderr.contains("retrying"), "INFO log on stderr: {stderr}");
    assert!(
        !stderr.contains('\u{1b}'),
        "no ANSI when stderr is a pipe: {stderr}"
    );
}

#[test]
fn malformed_base_url_exits_2_without_retrying() {
    let output = run("localhost:8317/v1", "bad-url", &["--json"]);
    assert_eq!(output.status.code(), Some(2), "{:?}", output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("GATEWAY_BASE_URL"));
}

#[test]
fn zero_deadline_and_low_cap_exit_2() {
    for args in [
        &["--deadline-secs", "0"][..],
        &["--max-turns", "10", "--max-turns-cap", "9"][..],
    ] {
        let output = run("http://127.0.0.1:9/v1", "args", args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
}

#[test]
fn a_rejected_request_exits_8() {
    let base = canned_gateway(vec![(
        400,
        r#"{"error":{"message":"unknown model m"}}"#.to_owned(),
    )]);
    let output = run(&base, "rejected", &["--json"]);
    assert_eq!(output.status.code(), Some(8), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown model m"));
}

/// A gateway that accepts and never answers ends the run inside the
/// deadline with exit 9: the per-attempt timeout is clamped to the time
/// left, so the deadline, not retry exhaustion, ends the run.
#[test]
fn a_hanging_gateway_ends_by_the_deadline_with_exit_9() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener binds");
    let addr = listener.local_addr().expect("address");
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    let started = Instant::now();
    let output = run(
        &format!("http://{addr}/v1"),
        "hang",
        &["--json", "--deadline-secs", "4"],
    );
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(9), "{output:?}");
    assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}
