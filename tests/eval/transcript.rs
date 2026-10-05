//! Gateway transcripts: a recording proxy for live runs and a replaying
//! stub for the offline gate.
//!
//! A transcript is `gateway.r<k>.jsonl` in a goal's fixture dir: one
//! [`Exchange`] per HTTP request the CLI made to the gateway, in order
//! (retries included). Replaying it to the same CLI with the same tool
//! fixture reproduces the run: the agent loop makes its gateway calls one
//! at a time and runs a turn's tool calls in order.

use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One gateway request and the response the CLI got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exchange {
    /// Request path, e.g. `/v1/messages`.
    pub path: String,
    /// `model` of the request body, when it had one.
    pub model: Option<String>,
    /// Size of the request body, for debugging a diverging replay.
    pub request_bytes: usize,
    pub status: u16,
    /// Response body, decoded text.
    pub body: String,
}

/// Transcript file name of repeat `k`.
pub fn file_name(repeat: u32) -> String {
    format!("gateway.r{repeat}.jsonl")
}

pub fn read(path: &Path) -> Result<Vec<Exchange>, String> {
    let file = std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    std::io::BufReader::new(file)
        .lines()
        .enumerate()
        .filter(|(_, line)| line.as_ref().map_or(true, |l| !l.trim().is_empty()))
        .map(|(index, line)| {
            let line = line.map_err(|err| format!("{}: {err}", path.display()))?;
            serde_json::from_str(&line)
                .map_err(|err| format!("{}:{}: {err}", path.display(), index + 1))
        })
        .collect()
}

/// A raw HTTP/1.1 request: request line + headers, then the body.
struct RawRequest {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Index of the end of the header block, and the declared body length.
fn header_end(seen: &[u8]) -> Option<(usize, usize)> {
    let end = seen.windows(4).position(|w| w == b"\r\n\r\n")?;
    let text = String::from_utf8_lossy(&seen[..end]);
    let length = text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    Some((end, length))
}

fn parse_raw(seen: &[u8], end: usize, length: usize) -> RawRequest {
    let head = String::from_utf8_lossy(&seen[..end]);
    let mut lines = head.lines();
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    let start = end + 4;
    let body = seen[start..(start + length).min(seen.len())].to_vec();
    RawRequest {
        path,
        headers,
        body,
    }
}

fn request_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("model")?
        .as_str()
        .map(str::to_owned)
}

fn response_bytes(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Headers the gateway client sends that the upstream needs.
const FORWARDED: [&str; 4] = [
    "authorization",
    "x-api-key",
    "anthropic-version",
    "content-type",
];

/// A live proxy in front of the real gateway that appends every exchange
/// to a transcript file. Dropping it stops accepting connections.
pub struct RecordingProxy {
    /// Base URL to hand the CLI as `GATEWAY_BASE_URL`.
    pub base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RecordingProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RecordingProxy {
    /// Proxy `upstream` (a `GATEWAY_BASE_URL`), recording into `out`
    /// (created, or truncated).
    pub async fn start(upstream: &str, out: PathBuf) -> Result<Self, String> {
        let upstream_url = url::Url::parse(upstream).map_err(|err| format!("{upstream}: {err}"))?;
        let origin = upstream_url.origin().ascii_serialization();
        let base_path = upstream_url.path().trim_end_matches('/').to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|err| err.to_string())?;
        let addr = listener.local_addr().map_err(|err| err.to_string())?;
        let file =
            std::fs::File::create(&out).map_err(|err| format!("{}: {err}", out.display()))?;
        let file = Arc::new(Mutex::new(file));
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .map_err(|err| err.to_string())?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let client = client.clone();
                let origin = origin.clone();
                let file = Arc::clone(&file);
                tokio::spawn(async move {
                    if let Err(err) = proxy_one(stream, &client, &origin, &file).await {
                        eprintln!("eval recording proxy: {err}");
                    }
                });
            }
        });
        Ok(Self {
            base_url: format!("http://{addr}{base_path}"),
            task,
        })
    }
}

async fn proxy_one(
    mut stream: tokio::net::TcpStream,
    client: &reqwest::Client,
    origin: &str,
    file: &Mutex<std::fs::File>,
) -> Result<(), String> {
    let mut seen = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let (end, length) = loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|err| err.to_string())?;
        if n == 0 {
            return Err("client closed before a full request".to_owned());
        }
        seen.extend_from_slice(&chunk[..n]);
        if let Some((end, length)) = header_end(&seen) {
            if seen.len() >= end + 4 + length {
                break (end, length);
            }
        }
    };
    let request = parse_raw(&seen, end, length);
    let mut upstream = client.post(format!("{origin}{}", request.path));
    for (name, value) in &request.headers {
        if FORWARDED.contains(&name.as_str()) {
            upstream = upstream.header(name.as_str(), value.as_str());
        }
    }
    let (status, body) = match upstream.body(request.body.clone()).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            (status, body)
        }
        // The CLI sees an upstream failure as a 502 and retries like any
        // 5xx; the transcript records the same 502.
        Err(err) => (
            502,
            serde_json::json!({"error": {"message": format!("eval proxy: {err}")}}).to_string(),
        ),
    };
    let exchange = Exchange {
        path: request.path,
        model: request_model(&request.body),
        request_bytes: request.body.len(),
        status,
        body,
    };
    {
        let mut line = serde_json::to_string(&exchange).map_err(|err| err.to_string())?;
        line.push('\n');
        let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write_all(line.as_bytes())
            .map_err(|err| err.to_string())?;
    }
    stream
        .write_all(&response_bytes(exchange.status, &exchange.body))
        .await
        .map_err(|err| err.to_string())?;
    stream.shutdown().await.map_err(|err| err.to_string())
}

/// A local gateway that answers the n-th request with the n-th recorded
/// response, one connection each. `served()` counts answered requests;
/// `path_mismatches()` those whose path differs from the recording.
pub struct ReplayStub {
    pub base_url: String,
    served: Arc<AtomicUsize>,
    path_mismatches: Arc<AtomicUsize>,
}

impl ReplayStub {
    pub fn spawn(exchanges: Vec<Exchange>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("stub listener binds");
        let addr = listener.local_addr().expect("stub address");
        let base_path = exchanges
            .first()
            .and_then(|first| first.path.rsplit_once('/').map(|(base, _)| base.to_owned()))
            .unwrap_or_default();
        let served = Arc::new(AtomicUsize::new(0));
        let path_mismatches = Arc::new(AtomicUsize::new(0));
        let (served_count, mismatches) = (Arc::clone(&served), Arc::clone(&path_mismatches));
        std::thread::spawn(move || {
            for exchange in exchanges {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut seen = Vec::new();
                let mut chunk = [0u8; 16 * 1024];
                let mut request_path = None;
                while let Ok(n) = stream.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&chunk[..n]);
                    if let Some((end, length)) = header_end(&seen) {
                        if seen.len() >= end + 4 + length {
                            request_path = Some(parse_raw(&seen, end, length).path);
                            break;
                        }
                    }
                }
                if request_path.as_deref() != Some(exchange.path.as_str()) {
                    mismatches.fetch_add(1, Ordering::SeqCst);
                }
                let _ = stream.write_all(&response_bytes(exchange.status, &exchange.body));
                served_count.fetch_add(1, Ordering::SeqCst);
            }
            // Dropping the listener here refuses any request past the
            // recording, so an over-long replay fails instead of hanging.
        });
        Self {
            base_url: format!("http://{addr}{base_path}"),
            served,
            path_mismatches,
        }
    }

    pub fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    pub fn path_mismatches(&self) -> usize {
        self.path_mismatches.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_serves_recorded_responses_in_order() {
        let stub = ReplayStub::spawn(vec![
            Exchange {
                path: "/v1/messages".to_owned(),
                model: Some("m".to_owned()),
                request_bytes: 2,
                status: 500,
                body: "{\"error\":{}}".to_owned(),
            },
            Exchange {
                path: "/v1/messages".to_owned(),
                model: Some("m".to_owned()),
                request_bytes: 2,
                status: 200,
                body: "{\"ok\":true}".to_owned(),
            },
        ]);
        assert!(stub.base_url.ends_with("/v1"), "{}", stub.base_url);
        let send = || {
            let host = stub.base_url.trim_start_matches("http://");
            let host = host.split('/').next().unwrap();
            let mut stream = std::net::TcpStream::connect(host).unwrap();
            stream
                .write_all(b"POST /v1/messages HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            reply
        };
        assert!(send().starts_with("HTTP/1.1 500"));
        let second = send();
        assert!(second.starts_with("HTTP/1.1 200"));
        assert!(second.ends_with("{\"ok\":true}"));
        assert_eq!(stub.served(), 2);
        assert_eq!(stub.path_mismatches(), 0);
    }
}
