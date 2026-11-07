//! HTTP CONNECT tunnel establishment, Basic Auth header injection, and rotation teardown.

use std::time::Duration;
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::pool::ProxyPool;

/// Process an incoming HTTP CONNECT tunnel request from a client (e.g. OpenCode CLI/Desktop).
///
/// Forwards upstream to the active proxy in the pool with Basic Auth injection, or directly
/// to the target if no proxy is configured. When a rotation occurs, active tunnels receive
/// a termination signal to force client reconnect.
pub async fn handle_connect_tunnel(mut client: TcpStream, pool: ProxyPool) {
    let mut req_buf = Vec::new();
    let mut temp = [0u8; 1024];

    // Read HTTP request headers until \r\n\r\n
    let target = loop {
        match client.read(&mut temp).await {
            Ok(0) => return,
            Ok(n) => {
                req_buf.extend_from_slice(&temp[..n]);
                if let Some(pos) = find_double_crlf(&req_buf) {
                    let header_str = String::from_utf8_lossy(&req_buf[..pos]);
                    let first_line = header_str.lines().next().unwrap_or("");
                    let parts: Vec<&str> = first_line.split_whitespace().collect();
                    if parts.len() >= 2 && parts[0].eq_ignore_ascii_case("CONNECT") {
                        break parts[1].to_string();
                    } else {
                        warn!("Invalid CONNECT request: {}", first_line);
                        let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
                        return;
                    }
                }
                if req_buf.len() > 8192 {
                    let _ = client
                        .write_all(b"HTTP/1.1 413 Payload Too Large\r\n\r\n")
                        .await;
                    return;
                }
            }
            Err(e) => {
                debug!("Client read error before CONNECT: {}", e);
                return;
            }
        }
    };

    let active_proxy = pool.get_active().await;

    match active_proxy {
        Some(proxy) => {
            let proxy_addr = format!("{}:{}", proxy.host, proxy.port);
            debug!("Tunneling CONNECT {} via proxy {}", target, proxy_addr);

            let mut upstream =
                match tokio::time::timeout(Duration::from_secs(8), TcpStream::connect(&proxy_addr))
                    .await
                {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(e)) => {
                        warn!("Failed to connect to upstream proxy {}: {}", proxy_addr, e);
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                        return;
                    }
                    Err(_) => {
                        warn!("Timeout connecting to upstream proxy {}", proxy_addr);
                        let _ = client
                            .write_all(b"HTTP/1.1 504 Gateway Timeout\r\n\r\n")
                            .await;
                        return;
                    }
                };

            // Build CONNECT request to upstream proxy
            let mut upstream_req = format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n", target, target);
            if let Some(auth_header) = proxy.basic_auth_header() {
                upstream_req.push_str(&format!("Proxy-Authorization: {}\r\n", auth_header));
            }
            upstream_req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");

            if let Err(e) = upstream.write_all(upstream_req.as_bytes()).await {
                warn!("Failed to write CONNECT to upstream proxy: {}", e);
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            }

            // Read response from upstream proxy
            let mut resp_buf = Vec::new();
            let mut u_temp = [0u8; 1024];

            let success = loop {
                match upstream.read(&mut u_temp).await {
                    Ok(0) => break false,
                    Ok(n) => {
                        resp_buf.extend_from_slice(&u_temp[..n]);
                        if let Some(pos) = find_double_crlf(&resp_buf) {
                            let resp_header = String::from_utf8_lossy(&resp_buf[..pos]);
                            let first_line = resp_header.lines().next().unwrap_or("");
                            if first_line.contains("200") {
                                break true;
                            } else {
                                warn!("Upstream proxy rejected CONNECT {}: {}", target, first_line);
                                break false;
                            }
                        }
                        if resp_buf.len() > 4096 {
                            break false;
                        }
                    }
                    Err(e) => {
                        warn!("Error reading upstream proxy CONNECT response: {}", e);
                        break false;
                    }
                }
            };

            if success {
                if let Err(e) = client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                {
                    debug!("Failed to send 200 Connection Established to client: {}", e);
                    return;
                }

                // Transfer buffered bytes from client and upstream proxy if any
                if let Some(req_end) = find_double_crlf(&req_buf) {
                    if req_end < req_buf.len() {
                        if let Err(e) = upstream.write_all(&req_buf[req_end..]).await {
                            debug!("Failed to write buffered client bytes to upstream: {}", e);
                            return;
                        }
                    }
                }
                if let Some(resp_end) = find_double_crlf(&resp_buf) {
                    if resp_end < resp_buf.len() {
                        if let Err(e) = client.write_all(&resp_buf[resp_end..]).await {
                            debug!("Failed to write buffered upstream bytes to client: {}", e);
                            return;
                        }
                    }
                }

                info!(
                    "Tunnel established: {} ↔ {} ({})",
                    target, proxy.host, proxy.provider
                );
                let mut rotation_rx = pool.subscribe_rotation();
                tokio::select! {
                    res = copy_bidirectional(&mut client, &mut upstream) => {
                        if let Err(e) = res {
                            debug!("Tunnel {} finished: {}", target, e);
                        }
                    }
                    Ok(new_idx) = rotation_rx.recv() => {
                        info!(
                            "Proxy rotated to index {}: aborting active CONNECT tunnel to {} to force client reconnection",
                            new_idx + 1,
                            target
                        );
                    }
                }
            } else {
                let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            }
        }
        None => {
            // Direct connect without upstream proxy
            debug!("Direct CONNECT tunnel to {}", target);
            let mut upstream =
                match tokio::time::timeout(Duration::from_secs(8), TcpStream::connect(&target))
                    .await
                {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(e)) => {
                        warn!("Direct connect to {} failed: {}", target, e);
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                        return;
                    }
                    Err(_) => {
                        warn!("Timeout connecting directly to {}", target);
                        let _ = client
                            .write_all(b"HTTP/1.1 504 Gateway Timeout\r\n\r\n")
                            .await;
                        return;
                    }
                };

            if let Err(e) = client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
            {
                debug!("Failed to send 200 Connection Established to client: {}", e);
                return;
            }

            if let Some(req_end) = find_double_crlf(&req_buf) {
                if req_end < req_buf.len() {
                    if let Err(e) = upstream.write_all(&req_buf[req_end..]).await {
                        debug!("Failed to write buffered client bytes to upstream: {}", e);
                        return;
                    }
                }
            }

            info!("Direct tunnel established: {}", target);
            let mut rotation_rx = pool.subscribe_rotation();
            tokio::select! {
                res = copy_bidirectional(&mut client, &mut upstream) => {
                    if let Err(e) = res {
                        debug!("Direct tunnel {} finished: {}", target, e);
                    }
                }
                Ok(new_idx) = rotation_rx.recv() => {
                    info!(
                        "Proxy rotated to index {}: aborting active direct CONNECT tunnel to {} to force client reconnection",
                        new_idx + 1,
                        target
                    );
                }
            }
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}
