//! HTTP CONNECT tunnel establishment, Basic Auth header injection, and rotation teardown.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{copy_bidirectional, AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpStream};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::pool::ProxyPool;
use crate::proxy::server::find_double_crlf;

const CLIENT_HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const UPSTREAM_HEADER_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_CLIENT_HEADER_BYTES: usize = 8_192;
const MAX_UPSTREAM_HEADER_BYTES: usize = 4_096;
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
const ERROR_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq)]
enum HeaderError {
    Eof,
    TooLarge,
    Timeout,
    Io,
}

async fn read_headers_bounded<S: AsyncRead + Unpin>(
    stream: &mut S,
    max_len: usize,
    timeout_dur: Duration,
) -> Result<(Vec<u8>, usize), HeaderError> {
    let read_fut = async {
        let mut buf = Vec::with_capacity(1024);
        let mut temp = [0u8; 1024];

        loop {
            let search_start = buf.len().saturating_sub(3);
            match stream.read(&mut temp).await {
                Ok(0) => return Err(HeaderError::Eof),
                Ok(n) => {
                    buf.extend_from_slice(&temp[..n]);
                    if let Some(relative_pos) = find_double_crlf(&buf[search_start..]) {
                        return Ok((buf, search_start + relative_pos));
                    }
                    if buf.len() > max_len {
                        return Err(HeaderError::TooLarge);
                    }
                }
                Err(_) => return Err(HeaderError::Io),
            }
        }
    };

    match tokio::time::timeout(timeout_dur, read_fut).await {
        Ok(result) => result,
        Err(_) => Err(HeaderError::Timeout),
    }
}

/// Validate a CONNECT target against SSRF and DNS-rebinding attacks.
pub async fn validate_connect_target(target: &str) -> bool {
    resolve_and_validate_target(target).await.is_ok()
}

async fn resolve_and_validate_target(target: &str) -> Result<Vec<SocketAddr>, &'static str> {
    if target.is_empty() {
        return Err("empty target");
    }
    if target.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err("invalid characters");
    }

    let (host, port_str) = target.rsplit_once(':').ok_or("missing port")?;
    let port = port_str.parse::<u16>().map_err(|_| "invalid port")?;
    if port == 0 {
        return Err("port zero");
    }

    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || host.len() > 253 {
        return Err("invalid host");
    }
    if host
        .bytes()
        .any(|b| matches!(b, b'/' | b'\\' | b'@' | b'?' | b'#' | b'%' | b'[' | b']'))
    {
        return Err("invalid host");
    }

    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        return Err("forbidden hostname");
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_forbidden_ip(ip) {
            return Err("forbidden address");
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let resolved = tokio::time::timeout(DNS_LOOKUP_TIMEOUT, lookup_host((host, port)))
        .await
        .map_err(|_| "dns timeout")?
        .map_err(|_| "dns failure")?
        .collect::<Vec<_>>();

    if resolved.is_empty() {
        return Err("no addresses");
    }
    if resolved.iter().any(|addr| is_forbidden_ip(addr.ip())) {
        return Err("forbidden resolved address");
    }

    Ok(resolved)
}

fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4(v4),
        IpAddr::V6(v6) => {
            if v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
            {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_forbidden_ipv4(v4);
            }
            let segments = v6.segments();
            segments[0] == 0x2001 && segments[1] == 0x0db8
        }
    }
}

fn is_forbidden_ipv4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_unspecified()
        || v4.is_documentation()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || octets[0] >= 240
}

async fn send_error(stream: &mut TcpStream, status: &str) {
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = tokio::time::timeout(ERROR_WRITE_TIMEOUT, stream.write_all(response.as_bytes())).await;
    let _ = stream.shutdown().await;
}

async fn relay_until_rotation(
    mut client: TcpStream,
    mut upstream: TcpStream,
    rotation_rx: &mut broadcast::Receiver<usize>,
    target: &str,
) {
    tokio::select! {
        res = copy_bidirectional(&mut client, &mut upstream) => {
            if let Err(e) = res {
                debug!("Tunnel {} finished: {}", target, e);
            }
        }
        rot = rotation_rx.recv() => {
            match rot {
                Ok(idx) => info!(
                    "Proxy rotated to index {}: aborting active CONNECT tunnel to {}",
                    idx + 1,
                    target
                ),
                Err(broadcast::error::RecvError::Lagged(n)) => warn!(
                    "Rotation receiver lagged by {} events: aborting active CONNECT tunnel to {}",
                    n, target
                ),
                Err(broadcast::error::RecvError::Closed) => warn!(
                    "Rotation channel closed: aborting active CONNECT tunnel to {}",
                    target
                ),
            }
            let _ = client.shutdown().await;
            let _ = upstream.shutdown().await;
        }
    }
}

/// Process an incoming HTTP CONNECT tunnel request from a client (e.g. OpenCode CLI/Desktop).
///
/// Forwards upstream to the active proxy in the pool with Basic Auth injection, or directly
/// to the target if no proxy is configured. When a rotation occurs, active tunnels receive
/// a termination signal to force client reconnect.
pub async fn handle_connect_tunnel(mut client: TcpStream, pool: ProxyPool) {
    // Subscribe before selecting a proxy to eliminate the TOCTOU rotation window.
    let mut rotation_rx = pool.subscribe_rotation();

    let (req_buf, req_end) =
        match read_headers_bounded(&mut client, MAX_CLIENT_HEADER_BYTES, CLIENT_HEADER_TIMEOUT)
            .await
        {
            Ok(result) => result,
            Err(HeaderError::TooLarge) => {
                send_error(&mut client, "413 Payload Too Large").await;
                return;
            }
            Err(HeaderError::Timeout) => {
                send_error(&mut client, "408 Request Timeout").await;
                return;
            }
            Err(HeaderError::Eof) | Err(HeaderError::Io) => return,
        };

    let header_str = String::from_utf8_lossy(&req_buf[..req_end]);
    let first_line = header_str.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();

    if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("CONNECT") {
        warn!("Invalid CONNECT request: {:?}", first_line);
        send_error(&mut client, "400 Bad Request").await;
        return;
    }

    let target = parts[1].to_string();
    let resolved = match resolve_and_validate_target(&target).await {
        Ok(addresses) => addresses,
        Err(reason) => {
            warn!("Rejected CONNECT target {:?}: {}", target, reason);
            send_error(&mut client, "403 Forbidden").await;
            return;
        }
    };

    let active_proxy = pool.get_active().await;

    match active_proxy {
        Some(proxy) => {
            if !proxy.protocol.eq_ignore_ascii_case("http") {
                warn!(
                    "Unsupported upstream proxy protocol '{}' for {}:{}",
                    proxy.protocol, proxy.host, proxy.port
                );
                send_error(&mut client, "502 Bad Gateway").await;
                return;
            }

            let proxy_addr = format!("{}:{}", proxy.host, proxy.port);
            debug!("Tunneling CONNECT {} via proxy {}", target, proxy_addr);

            let mut upstream = match tokio::time::timeout(
                UPSTREAM_CONNECT_TIMEOUT,
                TcpStream::connect((proxy.host.as_str(), proxy.port)),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    warn!("Failed to connect to upstream proxy {}: {}", proxy_addr, e);
                    send_error(&mut client, "502 Bad Gateway").await;
                    return;
                }
                Err(_) => {
                    warn!("Timeout connecting to upstream proxy {}", proxy_addr);
                    send_error(&mut client, "504 Gateway Timeout").await;
                    return;
                }
            };

            let mut upstream_req = format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n", target, target);
            if let Some(auth_header) = proxy.basic_auth_header() {
                upstream_req.push_str(&format!("Proxy-Authorization: {}\r\n", auth_header));
            }
            upstream_req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");

            let write_result = tokio::time::timeout(
                UPSTREAM_HEADER_TIMEOUT,
                upstream.write_all(upstream_req.as_bytes()),
            )
            .await;

            match write_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("Failed to write CONNECT to upstream proxy: {}", e);
                    send_error(&mut client, "502 Bad Gateway").await;
                    return;
                }
                Err(_) => {
                    warn!("Timeout writing CONNECT to upstream proxy {}", proxy_addr);
                    send_error(&mut client, "504 Gateway Timeout").await;
                    return;
                }
            }

            let (resp_buf, resp_end) = match read_headers_bounded(
                &mut upstream,
                MAX_UPSTREAM_HEADER_BYTES,
                UPSTREAM_HEADER_TIMEOUT,
            )
            .await
            {
                Ok(result) => result,
                Err(HeaderError::Timeout) => {
                    warn!("Timeout reading CONNECT response from {}", proxy_addr);
                    send_error(&mut client, "504 Gateway Timeout").await;
                    return;
                }
                Err(_) => {
                    send_error(&mut client, "502 Bad Gateway").await;
                    return;
                }
            };

            let resp_header = String::from_utf8_lossy(&resp_buf[..resp_end]);
            let status_line = resp_header.lines().next().unwrap_or("");
            let status_code = status_line.split_whitespace().nth(1).unwrap_or("");

            if status_code != "200" {
                warn!(
                    "Upstream proxy rejected CONNECT {}: {}",
                    target, status_line
                );
                send_error(&mut client, "502 Bad Gateway").await;
                return;
            }

            if client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }

            if req_end < req_buf.len() && upstream.write_all(&req_buf[req_end..]).await.is_err() {
                return;
            }
            if resp_end < resp_buf.len() && client.write_all(&resp_buf[resp_end..]).await.is_err() {
                return;
            }

            info!(
                "Tunnel established: {} ↔ {} ({})",
                target, proxy.host, proxy.provider
            );
            relay_until_rotation(client, upstream, &mut rotation_rx, &target).await;
        }
        None => {
            debug!("Direct CONNECT tunnel to {}", target);

            let mut upstream = match tokio::time::timeout(
                UPSTREAM_CONNECT_TIMEOUT,
                TcpStream::connect(&resolved[..]),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    warn!("Direct connect to {} failed: {}", target, e);
                    send_error(&mut client, "502 Bad Gateway").await;
                    return;
                }
                Err(_) => {
                    warn!("Timeout connecting directly to {}", target);
                    send_error(&mut client, "504 Gateway Timeout").await;
                    return;
                }
            };

            if client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }

            if req_end < req_buf.len() && upstream.write_all(&req_buf[req_end..]).await.is_err() {
                return;
            }

            info!("Direct tunnel established: {}", target);
            relay_until_rotation(client, upstream, &mut rotation_rx, &target).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn test_ssrf_target_validation() {
        let forbidden = [
            "127.0.0.1:8080",
            "localhost:3000",
            "169.254.169.254:80",
            "192.168.1.1:443",
            "10.0.0.1:80",
            "172.16.0.1:80",
            "100.64.0.1:80",
            "[::1]:8080",
            "[::ffff:127.0.0.1]:80",
            "evil.local:443",
            "evil.internal:443",
            "evil.com:443\r\nX-Injected: 1",
            "evil.com:443 X",
            "evil.com:0",
            "",
        ];

        for target in forbidden {
            assert!(
                !validate_connect_target(target).await,
                "target should be rejected: {target:?}"
            );
        }

        assert!(validate_connect_target("1.1.1.1:443").await);
        assert!(validate_connect_target("8.8.8.8:53").await);
    }

    #[tokio::test]
    async fn test_read_headers_bounded_timeout() {
        let (mut reader, _writer) = duplex(64);
        let result = read_headers_bounded(&mut reader, 1024, Duration::from_millis(20)).await;
        assert_eq!(result.unwrap_err(), HeaderError::Timeout);
    }

    #[tokio::test]
    async fn test_read_headers_bounded_too_large() {
        let (mut reader, mut writer) = duplex(4096);
        writer.write_all(&vec![b'a'; 2048]).await.unwrap();
        let result = read_headers_bounded(&mut reader, 1024, Duration::from_secs(1)).await;
        assert_eq!(result.unwrap_err(), HeaderError::TooLarge);
    }
}
