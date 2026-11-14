//! Tiny HTTP server and temp-dir helper for tests, so the dump/sync
//! pipeline can be exercised without touching the real wiki.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::WikiConfig;

pub struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Reply {
    pub fn html(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn json(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn bytes(content_type: &'static str, body: &[u8]) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_vec(),
        }
    }

    pub fn status(status: u16) -> Self {
        Self::html(status, "<html><body>error</body></html>")
    }
}

pub struct TestServer {
    pub base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    /// Serve `handler(path_and_query)` on 127.0.0.1 until the test ends.
    pub async fn start(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let log = Arc::clone(&requests);

        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&buf);
                    let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    log.lock().unwrap().push(target.clone());

                    let reply = handler(&target);
                    let head = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        reply.status,
                        reply.content_type,
                        reply.body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&reply.body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        Self { base, requests }
    }

    /// Paths (with query) requested so far.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// Config pointing at a `TestServer`: Portuguese wiki at the root, English
/// wiki under `/en`, no politeness delays, fast retries.
pub fn test_config(base: &str, assets_dir: &Path) -> WikiConfig {
    let mut cfg = WikiConfig::new(assets_dir.to_path_buf());
    cfg.pt_base = base.to_string();
    cfg.en_base = format!("{base}/en");
    cfg.page_delay = Duration::ZERO;
    cfg.api_delay = Duration::ZERO;
    cfg.retries = 2;
    cfg.retry_delay = Duration::from_millis(1);
    cfg
}

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "svdex-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
