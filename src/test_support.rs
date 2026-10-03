//! Crate-wide `#[cfg(test)]` helpers (ADR-0009): one `EnvGuard` for
//! serializing env-var-mutating tests and one canned HTTP server for the
//! raw-TCP client tests in `llm::client` and `research::run`.
//!
//! Module-local env-var guards that only ever clear one module's own key
//! list (`web::search::cache::test_support::EnvGuard`,
//! `web::search::governor::test_support::EnvGuard`) keep their own
//! no-argument call sites but delegate their lock-and-clear mechanics to
//! [`EnvGuard`] here, so the `Mutex`-plus-clear-every-key logic exists in
//! exactly one place.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Serializes tests that mutate process env vars and clears the given key
/// list on both acquisition and drop, so one test's override never leaks
/// into the next regardless of which side left it set.
pub(crate) struct EnvGuard {
    keys: Vec<&'static str>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    pub(crate) fn lock(keys: Vec<&'static str>) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env_keys(&keys);
        Self { keys, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        clear_env_keys(&self.keys);
    }
}

/// Remove every given env var. The shared clear-every-key mechanics
/// behind both [`EnvGuard`] and the module-local env-var guards
/// (`web::search::cache::test_support::EnvGuard`,
/// `web::search::governor::test_support::EnvGuard`), which keep their own
/// `Mutex` (so an unrelated module's tests never block on this one's
/// lock) but delegate the actual clearing here.
pub(crate) fn clear_env_keys(keys: &[&'static str]) {
    for key in keys {
        std::env::remove_var(key);
    }
}

/// One scripted HTTP response for [`CannedServer`]. `status`/`body` are
/// always sent; `headers` are appended verbatim after the standard
/// `Content-Type`/`Content-Length`/`Connection` lines; `delay_before_response`
/// sleeps (on the server thread, after reading the request) before writing
/// the reply, for timeout tests.
pub(crate) struct CannedResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(&'static str, String)>,
    pub(crate) body: String,
    pub(crate) delay_before_response: Duration,
}

impl CannedResponse {
    pub(crate) fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            delay_before_response: Duration::ZERO,
        }
    }

    /// A bare status with an empty JSON body (`"{}"`), for error-mapping
    /// tests that only care about the status line.
    pub(crate) fn status(status: u16) -> Self {
        Self::text(status, "{}")
    }

    pub(crate) fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub(crate) fn with_delay(mut self, delay: Duration) -> Self {
        self.delay_before_response = delay;
        self
    }
}

/// Serves scripted [`CannedResponse`]s over plain HTTP from a background
/// thread, one per accepted connection, in order. Captures every request's
/// raw bytes (headers and body), so both a hit count ([`CannedServer::hits`])
/// and a wire-body assertion ([`CannedServer::requests`]) read from the same
/// capture. Uses `std::net::TcpListener` so no new deps are needed.
pub(crate) struct CannedServer {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl CannedServer {
    pub(crate) fn spawn(responses: Vec<CannedResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        let addr = listener.local_addr().expect("listener has an address");
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let requests_in_thread = Arc::clone(&requests);
        let total = responses.len();
        std::thread::spawn(move || {
            for canned in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_request_capturing(&mut stream);
                // Count before writing: the client may read the reply and
                // assert on the hit count before a post-write push lands.
                let served = {
                    let mut requests = requests_in_thread.lock().expect("requests lock");
                    requests.push(request);
                    requests.len()
                };
                if !canned.delay_before_response.is_zero() {
                    std::thread::sleep(canned.delay_before_response);
                }
                let mut head = format!(
                    "HTTP/1.1 {} x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    canned.status,
                    canned.body.len()
                );
                for (name, value) in &canned.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                let _ = stream.write_all(format!("{head}\r\n{}", canned.body).as_bytes());
                if served >= total {
                    return;
                }
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    pub(crate) fn base(&self) -> &str {
        &self.base_url
    }

    /// Every captured request so far, raw bytes (headers + body) as UTF-8,
    /// in arrival order.
    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests lock").clone()
    }

    pub(crate) fn hits(&self) -> usize {
        self.requests.lock().expect("requests lock").len()
    }
}

fn read_request_capturing(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while let Ok(n) = stream.read(&mut chunk) {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(buf.len(), |i| i + 4);
    let content_length = String::from_utf8_lossy(&buf[..header_end])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    let mut remaining = content_length.saturating_sub(buf.len() - header_end);
    while remaining > 0 {
        let Ok(n) = stream.read(&mut chunk) else {
            break;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        remaining = remaining.saturating_sub(n);
    }
    String::from_utf8_lossy(&buf).into_owned()
}
