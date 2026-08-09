//! Test-only helpers. Compiled under `cfg(test)` only.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A one-file HTTP server that answers every connection with a canned response
/// (or, in [`CannedServer::silent`], never answers at all) and records what it
/// was sent.
///
/// Deliberately hand-rolled: the enroll/register tests need exactly two
/// behaviours — "reply with these bytes" and "accept and stall" — and the second
/// is the one a mock-server crate makes awkward.
pub struct CannedServer {
    /// `http://127.0.0.1:<port>` — pass straight in as `cloud_base`.
    pub base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl CannedServer {
    /// Reply to every request with `response` (raw HTTP, headers included).
    pub async fn responding(response: String) -> Self {
        Self::start(Some(response)).await
    }

    /// Accept connections, read the request, and then never reply — the hanging
    /// broker that used to freeze startup forever.
    pub async fn silent() -> Self {
        Self::start(None).await
    }

    async fn start(response: Option<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let log = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let response = response.clone();
                let log = Arc::clone(&log);
                tokio::spawn(async move { handle(sock, response, log).await });
            }
        });

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            task,
        }
    }

    /// The most recent request, verbatim (request line + headers + body).
    pub fn last_request(&self) -> String {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .cloned()
            .expect("server received no request")
    }
}

impl Drop for CannedServer {
    fn drop(&mut self) {
        // Otherwise the `silent` variant's connection handler outlives the test.
        self.task.abort();
    }
}

async fn handle(mut sock: TcpStream, response: Option<String>, log: Arc<Mutex<Vec<String>>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match sock.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if request_is_complete(&buf) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    log.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(String::from_utf8_lossy(&buf).into_owned());

    match response {
        Some(r) => {
            let _ = sock.write_all(r.as_bytes()).await;
            let _ = sock.flush().await;
        }
        // Hold the connection open and say nothing. The client's own timeout is
        // what has to break the deadlock — that is the thing under test.
        None => tokio::time::sleep(Duration::from_secs(60)).await,
    }
}

/// Have we read the headers and (if any) the whole `Content-Length` body?
fn request_is_complete(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    let Some(head_end) = text.find("\r\n\r\n") else {
        return false;
    };
    let head = &text[..head_end];
    let body_len = head
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    buf.len() >= head_end + 4 + body_len
}
