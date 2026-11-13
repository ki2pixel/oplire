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

const MAX_READ_CHUNK: usize = 64 * 1024;
const MAX_CARRY_BYTES: usize = 16 * 1024;
const LOG_POLL_MS: u64 = 500;

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

    let mut candidates = Vec::new();

    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(
            PathBuf::from(xdg)
                .join("opencode")
                .join("log")
                .join("opencode.log"),
        );
    }

    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidates.push(
            home.join(".local")
                .join("share")
                .join("opencode")
                .join("log")
                .join("opencode.log"),
        );
        candidates.push(
            home.join("Library")
                .join("Application Support")
                .join("opencode")
                .join("log")
                .join("opencode.log"),
        );
    }

    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local_app_data)
                .join("opencode")
                .join("log")
                .join("opencode.log"),
        );
    }

    if let Some(app_data) = std::env::var_os("APPDATA") {
        candidates.push(
            PathBuf::from(app_data)
                .join("opencode")
                .join("log")
                .join("opencode.log"),
        );
    }

    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let profile = PathBuf::from(profile);
        candidates.push(
            profile
                .join("AppData")
                .join("Local")
                .join("opencode")
                .join("log")
                .join("opencode.log"),
        );
        candidates.push(
            profile
                .join("AppData")
                .join("Roaming")
                .join("opencode")
                .join("log")
                .join("opencode.log"),
        );
    }

    candidates.into_iter().find(|path| path.exists())
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

fn read_chunk_len(file_len: u64, offset: u64) -> usize {
    (file_len.saturating_sub(offset)).min(MAX_READ_CHUNK as u64) as usize
}

fn scan_carry(carry: &mut String) -> bool {
    let mut triggered = false;

    while let Some(pos) = carry.find('\n') {
        if is_rate_limit_line(&carry[..pos]) {
            triggered = true;
            break;
        }
        carry.drain(..=pos);
    }

    if !triggered && carry.len() > MAX_CARRY_BYTES {
        triggered = is_rate_limit_line(carry);
        carry.clear();
    }

    triggered
}

async fn unlock_opencode_desktop_state() {
    let mut candidate_dbs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        candidate_dbs.push(
            home.join(".config")
                .join("ai.opencode.desktop")
                .join("drafts.sqlite"),
        );
        candidate_dbs.push(
            home.join("Library")
                .join("Application Support")
                .join("ai.opencode.desktop")
                .join("drafts.sqlite"),
        );
    }
    if let Some(app_data) = std::env::var_os("APPDATA") {
        candidate_dbs.push(
            PathBuf::from(app_data)
                .join("ai.opencode.desktop")
                .join("drafts.sqlite"),
        );
    }

    for db in candidate_dbs {
        if db.exists() {
            let _ = tokio::process::Command::new("sqlite3")
                .arg(&db)
                .arg("UPDATE state SET value = '{\"resetAt\":null}' WHERE name = 'opencode.global.dat' AND key = 'free-limit';")
                .output()
                .await;
        }
    }
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
                unlock_opencode_desktop_state().await;
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

            if let Err(e) = monitor_log_file(path, pool.clone(), debounce_ms).await {
                warn!(
                    "Log monitoring failed ({}). Falling back to HTTP polling on {}",
                    e, upstream
                );
                println!(
                    "{} Unable to read OpenCode log. Falling back to HTTP check on {}...",
                    "[INFO]".cyan(),
                    upstream
                );
                monitor_http_upstream(upstream, pool, debounce_ms).await
            } else {
                Ok(())
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

    let mut buffer = vec![0u8; 8192];
    let mut carry = String::new();

    loop {
        tokio::time::sleep(Duration::from_millis(LOG_POLL_MS)).await;

        let meta = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let file_len = meta.len();

        #[cfg(unix)]
        let rotated_inode = {
            use std::os::unix::fs::MetadataExt;
            match file.metadata().await {
                Ok(open_meta) => open_meta.ino() != meta.ino() || open_meta.dev() != meta.dev(),
                Err(_) => false,
            }
        };
        #[cfg(not(unix))]
        let rotated_inode = false;

        if file_len < offset || rotated_inode {
            match File::open(&path).await {
                Ok(new_file) => {
                    info!("Log file rotated or truncated, reopening and resetting offset to 0");
                    file = new_file;
                    offset = 0;
                    carry.clear();
                }
                Err(e) => {
                    warn!(
                        "Failed to reopen rotated log file {}: {}",
                        path.display(),
                        e
                    );
                    continue;
                }
            }
        }

        if file_len <= offset {
            continue;
        }

        let delta = read_chunk_len(file_len, offset);
        if delta == 0 {
            continue;
        }
        buffer.resize(delta, 0);

        if let Err(e) = file.seek(SeekFrom::Start(offset)).await {
            warn!("Seek error in log file: {}", e);
            continue;
        }

        match file.read(&mut buffer[..delta]).await {
            Ok(0) => {
                // The descriptor still points at an old, renamed inode.
                match File::open(&path).await {
                    Ok(new_file) => {
                        file = new_file;
                        offset = 0;
                        carry.clear();
                    }
                    Err(e) => {
                        warn!("Failed to reopen log file after EOF: {}", e);
                    }
                }
            }
            Ok(n) => {
                offset += n as u64;
                carry.push_str(&String::from_utf8_lossy(&buffer[..n]));

                if scan_carry(&mut carry) {
                    carry.clear();
                    rotate_proxy(&pool).await;
                    tokio::time::sleep(Duration::from_millis(debounce_ms)).await;

                    match File::open(&path).await {
                        Ok(mut new_file) => {
                            offset = new_file.seek(SeekFrom::End(0)).await.unwrap_or(0);
                            file = new_file;
                        }
                        Err(e) => {
                            warn!("Failed to reopen log file after rotation: {}", e);
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Error reading new bytes from log file: {}", e);
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
        .no_proxy()
        .connect_timeout(Duration::from_secs(3))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_carry_detects_fragmented_429() {
        let mut carry = String::from("some log line status=");
        assert!(!scan_carry(&mut carry));
        carry.push_str("429\n");
        assert!(scan_carry(&mut carry));
    }

    #[test]
    fn test_scan_carry_overflow_guard() {
        let mut carry = String::from("status=429 ");
        carry.push_str(&"x".repeat(MAX_CARRY_BYTES));
        assert!(!carry.contains('\n'));
        assert!(scan_carry(&mut carry));
    }

    #[test]
    fn test_read_chunk_len_is_bounded() {
        assert_eq!(read_chunk_len(u64::MAX, 0), MAX_READ_CHUNK);
        assert_eq!(read_chunk_len(10, 4), 6);
        assert_eq!(read_chunk_len(4, 10), 0);
    }

    #[test]
    fn test_is_rate_limit_patterns() {
        assert!(is_rate_limit_line("HTTP status=429"));
        assert!(is_rate_limit_line("quotaExceeded"));
        assert!(!is_rate_limit_line("all good"));
    }
}
