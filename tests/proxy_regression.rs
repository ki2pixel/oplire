//! End-to-end regression tests for the local forward proxy server.

use std::net::SocketAddr;
use std::time::Duration;

use oplire_reset::proxy::serve_proxy;
use oplire_reset::ProxyPool;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct TestServer {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    _dir: TempDir,
}

impl TestServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pool = ProxyPool::with_config_dir(Vec::new(), dir.path().join("oplire"));
        let handle = tokio::spawn(serve_proxy(listener, pool));

        Self {
            addr,
            handle,
            _dir: dir,
        }
    }

    async fn request(&self, request: &[u8]) -> Vec<u8> {
        let mut client = TcpStream::connect(self.addr).await.unwrap();
        client.write_all(request).await.unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
            .await
            .expect("proxy response timed out")
            .unwrap();

        response
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn split_response(response: &[u8]) -> (String, Vec<u8>) {
    let text = String::from_utf8_lossy(response);
    let (headers, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (headers.to_string(), body.as_bytes().to_vec())
}

#[tokio::test]
async fn test_head_health_has_no_body() {
    let server = TestServer::start().await;
    let response = server
        .request(b"HEAD /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await;
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 200 OK"), "{headers}");
    assert!(headers.contains("Content-Length: 2"), "{headers}");
    assert!(headers.contains("Connection: close"), "{headers}");
    assert!(body.is_empty(), "HEAD response must not include body bytes");
}

#[tokio::test]
async fn test_get_health_has_body() {
    let server = TestServer::start().await;
    let response = server
        .request(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await;
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 200 OK"), "{headers}");
    assert_eq!(body, b"OK");
}

#[tokio::test]
async fn test_connect_loopback_is_rejected() {
    let server = TestServer::start().await;
    let response = server
        .request(b"CONNECT 127.0.0.1:8080 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n")
        .await;
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 403 Forbidden"), "{headers}");
    assert!(headers.contains("Content-Length: 0"), "{headers}");
    assert!(headers.contains("Connection: close"), "{headers}");
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_connect_localhost_is_rejected() {
    let server = TestServer::start().await;
    let response = server
        .request(b"CONNECT localhost:3000 HTTP/1.1\r\nHost: localhost:3000\r\n\r\n")
        .await;
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 403 Forbidden"), "{headers}");
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_lowercase_connect_is_recognized_and_rejected() {
    let server = TestServer::start().await;
    let response = server
        .request(b"connect 127.0.0.1:8080 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n")
        .await;
    let (headers, _body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 403 Forbidden"), "{headers}");
}

#[tokio::test]
async fn test_invalid_plain_request_returns_400() {
    let server = TestServer::start().await;
    let response = server
        .request(b"POST /invalid HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await;
    let (headers, body) = split_response(&response);

    assert!(headers.starts_with("HTTP/1.1 400 Bad Request"), "{headers}");
    assert!(headers.contains("Content-Length: 0"), "{headers}");
    assert!(body.is_empty());
}
