//! TCP listener handling incoming HTTP CONNECT requests and plain HTTP health checks.

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::info;

use crate::pool::ProxyPool;
use crate::proxy::tunnel::handle_connect_tunnel;

async fn is_connect_request(socket: &TcpStream) -> bool {
    let mut peek_buf = [0u8; 7];
    let check = async {
        loop {
            match socket.peek(&mut peek_buf).await {
                Ok(0) => return false,
                Ok(n) if n >= 7 => return &peek_buf[..7] == b"CONNECT",
                Ok(n) => {
                    if peek_buf[..n] != b"CONNECT"[..n] {
                        return false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(_) => return false,
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_millis(250), check)
        .await
        .unwrap_or(false)
}

async fn handle_plain_http(mut socket: TcpStream) {
    let mut buf = Vec::new();
    let mut temp = [0u8; 1024];

    loop {
        match socket.read(&mut temp).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&temp[..n]);
                if find_double_crlf(&buf).is_some() || buf.len() > 8192 {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let first_line = head.lines().next().unwrap_or("");
    let is_health =
        first_line.starts_with("GET /health ") || first_line.starts_with("HEAD /health ");

    let response = if is_health {
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nOK"
    } else {
        "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    };

    let _ = socket.write_all(response.as_bytes()).await;
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Bind to the specified listen address and start accepting HTTP CONNECT proxy traffic.
pub async fn start_proxy_server(
    listen_addr: String,
    pool: Option<ProxyPool>,
) -> anyhow::Result<()> {
    let addr: SocketAddr = listen_addr
        .parse()
        .unwrap_or_else(|_| "127.0.0.1:8080".parse().unwrap());

    let pool = pool.unwrap_or_default();

    info!("Starting forward proxy server on {}", addr);
    if let Some(active) = pool.get_active().await {
        info!(
            "Active outbound ISP proxy: {} ({}:{})",
            active.provider, active.host, active.port
        );
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Proxy server listening on http://{}", addr);

    loop {
        let (socket, _remote_addr) = listener.accept().await?;
        let pool_clone = pool.clone();

        tokio::spawn(async move {
            if is_connect_request(&socket).await {
                handle_connect_tunnel(socket, pool_clone).await;
            } else {
                handle_plain_http(socket).await;
            }
        });
    }
}
