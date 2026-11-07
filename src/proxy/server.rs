//! TCP listener handling incoming HTTP CONNECT requests and plain HTTP health checks.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::pool::ProxyPool;
use crate::proxy::tunnel::handle_connect_tunnel;

const MAX_CONCURRENT_CONNECTIONS: usize = 512;
const PLAIN_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PLAIN_HEADER_BYTES: usize = 8 * 1024;
const CONNECT_PREFIX: &[u8; 8] = b"CONNECT ";

#[derive(Debug, PartialEq, Eq)]
enum PlainReadError {
    Timeout,
    TooLarge,
    Closed,
    Io,
}

pub(crate) fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

async fn is_connect_request(socket: &TcpStream) -> bool {
    let mut peek_buf = [0u8; 8];
    let check = async {
        loop {
            match socket.peek(&mut peek_buf).await {
                Ok(0) => return false,
                Ok(n) if n >= CONNECT_PREFIX.len() => {
                    return peek_buf[..].eq_ignore_ascii_case(CONNECT_PREFIX);
                }
                Ok(n) => {
                    if !peek_buf[..n].eq_ignore_ascii_case(&CONNECT_PREFIX[..n]) {
                        return false;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(_) => return false,
            }
        }
    };
    tokio::time::timeout(Duration::from_millis(250), check)
        .await
        .unwrap_or(false)
}

async fn write_and_close(socket: &mut TcpStream, response: &[u8]) {
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.write_all(response)).await;
    let _ = socket.shutdown().await;
}

async fn handle_plain_http(mut socket: TcpStream) {
    let mut buf = Vec::with_capacity(512);
    let mut temp = [0u8; 1024];

    let read_result = tokio::time::timeout(PLAIN_HTTP_TIMEOUT, async {
        loop {
            match socket.read(&mut temp).await {
                Ok(0) => return Err(PlainReadError::Closed),
                Ok(n) => {
                    buf.extend_from_slice(&temp[..n]);
                    if find_double_crlf(&buf).is_some() {
                        return Ok(());
                    }
                    if buf.len() > MAX_PLAIN_HEADER_BYTES {
                        return Err(PlainReadError::TooLarge);
                    }
                }
                Err(_) => return Err(PlainReadError::Io),
            }
        }
    })
    .await;

    let result = match read_result {
        Ok(result) => result,
        Err(_) => Err(PlainReadError::Timeout),
    };

    match result {
        Ok(()) => {
            let head = String::from_utf8_lossy(&buf);
            let mut parts = head.lines().next().unwrap_or("").split_whitespace();
            let method = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("");

            let response: &[u8] = match (method, path) {
                ("HEAD", "/health") => b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                ("GET", "/health") => b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nOK",
                _ => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            };
            write_and_close(&mut socket, response).await;
        }
        Err(PlainReadError::Timeout) => {
            write_and_close(
                &mut socket,
                b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        }
        Err(PlainReadError::TooLarge) => {
            write_and_close(
                &mut socket,
                b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        }
        Err(PlainReadError::Closed) | Err(PlainReadError::Io) => {
            let _ = socket.shutdown().await;
        }
    }
}

/// Normalize and parse a listen address, falling back to `127.0.0.1:8080`.
pub fn parse_listen_addr(raw: &str) -> SocketAddr {
    let trimmed = raw.trim();
    let normalized = match trimmed.strip_prefix(':') {
        Some(port) => format!("127.0.0.1:{port}"),
        None => trimmed.to_string(),
    };

    normalized.parse().unwrap_or_else(|e| {
        warn!(
            "Invalid listen address '{} ({})', falling back to 127.0.0.1:8080",
            raw, e
        );
        SocketAddr::from(([127, 0, 0, 1], 8080))
    })
}

/// Accept connections from an already-bound listener with bounded concurrency.
pub async fn serve_proxy(listener: TcpListener, pool: ProxyPool) -> anyhow::Result<()> {
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));

    loop {
        let (socket, remote_addr) = match listener.accept().await {
            Ok(connection) => connection,
            Err(e) => {
                warn!("Transient TCP accept error: {}", e);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!(
                    "Connection limit ({}) reached; dropping connection from {}",
                    MAX_CONCURRENT_CONNECTIONS, remote_addr
                );
                drop(socket);
                continue;
            }
        };

        let pool_clone = pool.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if is_connect_request(&socket).await {
                handle_connect_tunnel(socket, pool_clone).await;
            } else {
                handle_plain_http(socket).await;
            }
        });
    }
}

/// Bind to the specified listen address and start accepting HTTP CONNECT proxy traffic.
pub async fn start_proxy_server(
    listen_addr: String,
    pool: Option<ProxyPool>,
) -> anyhow::Result<()> {
    let addr = parse_listen_addr(&listen_addr);
    if !addr.ip().is_loopback() {
        warn!(
            "SECURITY WARNING: Proxy server is binding to non-loopback address {} without authentication!",
            addr
        );
    }

    let pool = pool.unwrap_or_default();

    info!("Starting forward proxy server on {}", addr);
    if let Some(active) = pool.get_active().await {
        info!(
            "Active outbound ISP proxy: {} ({}:{})",
            active.provider, active.host, active.port
        );
    }

    let listener = TcpListener::bind(addr).await?;
    info!("Proxy server listening on http://{}", addr);

    serve_proxy(listener, pool).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_listen_addr_normalizes_port_only() {
        assert_eq!(
            parse_listen_addr(":8080"),
            SocketAddr::from(([127, 0, 0, 1], 8080))
        );
        assert_eq!(
            parse_listen_addr("0.0.0.0:9000"),
            SocketAddr::from(([0, 0, 0, 0], 9000))
        );
        assert_eq!(
            parse_listen_addr("not-an-address"),
            SocketAddr::from(([127, 0, 0, 1], 8080))
        );
    }

    #[test]
    fn test_find_double_crlf() {
        let request = b"GET / HTTP/1.1\r\n\r\n";
        assert_eq!(find_double_crlf(request), Some(request.len()));
        assert_eq!(find_double_crlf(b"GET / HTTP/1.1\r\n"), None);
    }
}
