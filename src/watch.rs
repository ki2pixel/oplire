//! Real-time OpenCode log monitoring and automatic proxy pool rotation upon rate-limit detection.

use colored::Colorize;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::interval;
use tracing::{info, warn};

use crate::pool::ProxyPool;

/// Default debounce delay (in milliseconds) after a rotation event before resuming log checks.
pub const DEFAULT_DEBOUNCE_MS: u64 = 5_000;

/// Locate the OpenCode log file on the local system, supporting Linux, macOS, and Windows.
///
/// If a custom path is supplied and exists, it takes precedence. Otherwise standard OS
/// application data locations are searched.
pub fn find_opencode_log_path(custom: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(custom_path) = custom {
        if custom_path.exists() {
            return Some(custom_path);
        }
    }

    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        let path = PathBuf::from(xdg).join("opencode/log/opencode.log");
        if path.exists() {
            return Some(path);
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let p1 = PathBuf::from(&home).join(".local/share/opencode/log/opencode.log");
        if p1.exists() {
            return Some(p1);
        }
        let p2 = PathBuf::from(&home).join("Library/Application Support/opencode/log/opencode.log");
        if p2.exists() {
            return Some(p2);
        }
    }

    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data).join("opencode\\log\\opencode.log");
        if p.exists() {
            return Some(p);
        }
    }

    if let Ok(app_data) = std::env::var("APPDATA") {
        let p = PathBuf::from(app_data).join("opencode\\log\\opencode.log");
        if p.exists() {
            return Some(p);
        }
    }

    None
}

fn is_rate_limit_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("rate limit exceeded")
        || lower.contains("quotaexceeded")
        || lower.contains("ai.error: rate limit")
        || lower.contains("free usage limit reached")
        || lower.contains("used today's free messages")
        || lower.contains("today's free messages")
        || lower.contains("subscribe to opencode go")
        || lower.contains("status=429")
        || lower.contains("status: 429")
        || lower.contains("error: 429")
}

async fn rotate_proxy(pool: &Option<ProxyPool>) -> bool {
    match pool {
        Some(pool) => match pool.rotate().await {
            Some(next) => {
                println!(
                    "{} Switched to next ISP proxy: {} ({}:{})\n",
                    "[ROTATED]".green().bold(),
                    next.provider,
                    next.host,
                    next.port
                );
                true
            }
            None => {
                println!(
                    "{} No alternative proxy available in the pool, keeping the active one.\n",
                    "[WARN]".yellow().bold()
                );
                false
            }
        },
        None => {
            println!(
                "{} No proxy pool configured, cannot rotate.\n",
                "[WARN]".yellow().bold()
            );
            false
        }
    }
}

/// Start log monitoring (or fallback HTTP upstream polling) to rotate proxies when rate limits occur.
pub async fn start_watch_mode(
    upstream: &str,
    log_file: Option<PathBuf>,
    pool: Option<ProxyPool>,
    debounce_ms: u64,
) -> anyhow::Result<()> {
    let log_path = find_opencode_log_path(log_file);

    if let Some(ref p) = pool {
        if let Some(active) = p.get_active().await {
            println!(
                "{} Proxy pool active: {} ({}:{})",
                "✓".green().bold(),
                active.provider,
                active.host,
                active.port
            );
        }
    }

    match log_path {
        Some(path) => {
            info!("OpenCode log found at: {}", path.display());
            println!(
                "{} Attached to OpenCode log: {}",
                "✓".green().bold(),
                path.display().to_string().bold()
            );

            tokio::select! {
                res = monitor_log_file(path, pool.clone(), debounce_ms) => res,
                res = monitor_http_upstream(upstream, pool, debounce_ms) => res,
            }
        }
        None => {
            warn!(
                "No OpenCode log file detected. Falling back to HTTP polling on {}",
                upstream
            );
            println!(
                "{} OpenCode log file not found. Falling back to HTTP check on {}...",
                "[INFO]".cyan(),
                upstream
            );
            monitor_http_upstream(upstream, pool, debounce_ms).await
        }
    }
}

async fn monitor_log_file(
    path: PathBuf,
    pool: Option<ProxyPool>,
    debounce_ms: u64,
) -> anyhow::Result<()> {
    let mut file = File::open(&path).await?;
    let mut offset = file.seek(SeekFrom::End(0)).await.unwrap_or(0);
    info!(
        "Attached to log file at offset {} bytes (ignoring past history)",
        offset
    );

    let mut buffer = Vec::new();

    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };

        let file_len = meta.len();
        if file_len < offset {
            info!("Log file truncated or rotated, resetting offset to 0");
            offset = 0;
            let _ = file.seek(SeekFrom::Start(0)).await;
        }

        if file_len > offset {
            let bytes_to_read = (file_len - offset) as usize;
            buffer.resize(bytes_to_read, 0);

            if let Err(e) = file.seek(SeekFrom::Start(offset)).await {
                warn!("Seek error in log file: {}", e);
                continue;
            }

            match file.read_exact(&mut buffer).await {
                Ok(_) => {
                    offset = file_len;
                    let text = String::from_utf8_lossy(&buffer);
                    let mut triggered = false;

                    for line in text.lines() {
                        if is_rate_limit_line(line) {
                            warn!("OpenCode limit detected in log: {}", line.trim());
                            println!(
                                "\n{} Limit detected in OpenCode log!",
                                "[WARN]".yellow().bold()
                            );
                            triggered = true;
                            break;
                        }
                    }

                    if triggered {
                        rotate_proxy(&pool).await;

                        // Debounce: sleep and skip past any further lines emitted during rotation
                        tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
                        if let Ok(m) = tokio::fs::metadata(&path).await {
                            offset = m.len();
                            let _ = file.seek(SeekFrom::Start(offset)).await;
                        }
                    }
                }
                Err(e) => {
                    warn!("Error reading new bytes from log file: {}", e);
                }
            }
        }
    }
}

async fn monitor_http_upstream(
    upstream: &str,
    pool: Option<ProxyPool>,
    debounce_ms: u64,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let models_url = format!("{}/v1/models", upstream.trim_end_matches('/'));
    let mut check_interval = interval(Duration::from_secs(30));
    let mut consecutive_429 = 0u32;

    loop {
        check_interval.tick().await;

        match client.get(&models_url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    consecutive_429 = 0;
                } else if status.as_u16() == 429 {
                    consecutive_429 += 1;
                    warn!(
                        "HTTP upstream 429 detected (count: {}). Rotating proxy...",
                        consecutive_429
                    );
                    println!(
                        "\n{} Upstream returned HTTP 429! Rotating proxy...",
                        "[WARN]".yellow().bold()
                    );
                    rotate_proxy(&pool).await;
                    tokio::time::sleep(Duration::from_millis(debounce_ms)).await;
                }
            }
            Err(_) => {
                // Quietly ignore network connect errors to upstream if service is offline
            }
        }
    }
}
