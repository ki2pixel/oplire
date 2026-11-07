//! Proxy pool management, format parsing, file persistence, and rotation mechanisms.

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::info;

const FS_CHECK_INTERVAL_MS: u64 = 1_000;

/// Representation of a single proxy upstream destination.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProxyEntry {
    /// Proxy IP or domain hostname
    pub host: String,
    /// Proxy listen port
    pub port: u16,
    /// Optional Basic Auth username
    pub username: Option<String>,
    /// Optional Basic Auth password
    pub password: Option<String>,
    /// Inferred or labeled provider name (e.g. `proxiware-isp`, `webshare`, `custom`)
    pub provider: String,
    /// Proxy protocol scheme (e.g. `"http"` or `"socks5"`)
    pub protocol: String, // "http" or "socks5"
}

impl std::fmt::Debug for ProxyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyEntry")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field(
                "password",
                &self.password.as_ref().map(|_| "***REDACTED***"),
            )
            .field("provider", &self.provider)
            .field("protocol", &self.protocol)
            .finish()
    }
}

impl ProxyEntry {
    /// Parse a proxy line from various formats:
    /// - `host:port:username:password`
    /// - `host:port`
    /// - `http://user:pass@host:port` or `socks5://...`
    /// - With optional inline comment/tag: `host:port:user:pass # label`
    pub fn parse(line: &str, default_provider: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        // Only a whitespace-prefixed '#' starts a tag, so passwords may contain '#'.
        let (clean_line, tag) = if let Some((before, after)) = line.split_once(" #") {
            (before.trim(), Some(after.trim()))
        } else {
            (line, None)
        };

        let infer_provider = |host: &str, port: u16| -> String {
            if let Some(t) = tag {
                if !t.is_empty() {
                    return t.to_string();
                }
            }
            if default_provider != "custom" && !default_provider.is_empty() {
                return default_provider.to_string();
            }
            if port == 1337
                || host.contains("proxiware")
                || host.starts_with("188.220")
                || host.starts_with("51.241")
            {
                "proxiware-isp".to_string()
            } else if port == 8000 || host.contains("webshare") {
                "webshare".to_string()
            } else {
                "custom".to_string()
            }
        };

        // Case 1: URL format (http://user:pass@host:port or socks5://...)
        if clean_line.contains("://") {
            if let Ok(url) = url::Url::parse(clean_line) {
                if let Some(host) = url.host_str() {
                    let port = url.port().unwrap_or(80);
                    let username = if url.username().is_empty() {
                        None
                    } else {
                        Some(url.username().to_string())
                    };
                    let password = url.password().map(|p| p.to_string());
                    let protocol = url.scheme().to_string();
                    let provider = infer_provider(host, port);

                    return Some(Self {
                        host: host.to_string(),
                        port,
                        username,
                        password,
                        provider,
                        protocol,
                    });
                }
            }
        }

        // Case 2: host:port:user:pass (Standard Proxiware & Webshare format).
        // splitn(4) preserves passwords containing ':'.
        let parts: Vec<&str> = clean_line.splitn(4, ':').collect();
        match parts.len() {
            4 => {
                let host = parts[0].to_string();
                let port = parts[1].parse::<u16>().ok()?;
                let username = Some(parts[2].to_string());
                let password = Some(parts[3].to_string());
                let provider = infer_provider(&host, port);
                Some(Self {
                    host,
                    port,
                    username,
                    password,
                    provider,
                    protocol: "http".to_string(),
                })
            }
            2 => {
                let host = parts[0].to_string();
                let port = parts[1].parse::<u16>().ok()?;
                let provider = infer_provider(&host, port);
                Some(Self {
                    host,
                    port,
                    username: None,
                    password: None,
                    provider,
                    protocol: "http".to_string(),
                })
            }
            _ => None,
        }
    }

    pub fn url(&self) -> String {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => {
                format!(
                    "{}://{}:{}@{}:{}",
                    self.protocol, u, p, self.host, self.port
                )
            }
            (Some(u), None) => {
                format!("{}://{}@{}:{}", self.protocol, u, self.host, self.port)
            }
            _ => format!("{}://{}:{}", self.protocol, self.host, self.port),
        }
    }

    pub fn to_line(&self) -> String {
        let base = match (&self.username, &self.password) {
            (Some(u), Some(p)) => format!("{}:{}:{}:{}", self.host, self.port, u, p),
            _ => format!("{}:{}", self.host, self.port),
        };
        if !self.provider.is_empty() && self.provider != "custom" {
            format!("{} # {}", base, self.provider)
        } else {
            base
        }
    }

    pub fn basic_auth_header(&self) -> Option<String> {
        match (&self.username, &self.password) {
            (Some(u), Some(p)) => {
                let credentials = format!("{}:{}", u, p);
                Some(format!("Basic {}", BASE64.encode(credentials.as_bytes())))
            }
            _ => None,
        }
    }
}

/// In-memory proxy pool with concurrent access, file syncing, and rotation broadcasting.
#[derive(Debug, Clone)]
pub struct ProxyPool {
    /// Thread-safe list of configured proxies
    pub proxies: Arc<RwLock<Vec<ProxyEntry>>>,
    /// Atomic index of the currently active proxy
    pub active_index: Arc<AtomicUsize>,
    /// Last recorded modification timestamp of proxies.txt
    pub last_mtime: Arc<RwLock<Option<SystemTime>>>,
    /// Last recorded modification timestamp of active_proxy.idx
    pub last_index_mtime: Arc<RwLock<Option<SystemTime>>>,
    /// Broadcast channel transmitting index change events on rotation
    pub rotation_tx: tokio::sync::broadcast::Sender<usize>,
    /// Directory holding proxies.txt, active_proxy.idx and config.json
    config_dir: Arc<PathBuf>,
    /// Throttles filesystem metadata checks to at most once per second
    last_fs_check_ms: Arc<AtomicU64>,
}

impl Default for ProxyPool {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ProxyPool {
    /// Create a new ProxyPool with initial proxies and load persistent active index if available.
    pub fn new(proxies: Vec<ProxyEntry>) -> Self {
        Self::with_config_dir(proxies, Self::config_dir())
    }

    /// Create a pool backed by an explicit configuration directory.
    ///
    /// This keeps tests hermetic and lets embedders avoid the default user profile.
    pub fn with_config_dir(proxies: Vec<ProxyEntry>, config_dir: PathBuf) -> Self {
        let config_dir = Arc::new(config_dir);
        let proxies_path = config_dir.join("proxies.txt");
        let idx_path = config_dir.join("active_proxy.idx");
        let saved_idx = read_saved_index_from(&idx_path);
        let idx = if proxies.is_empty() {
            0
        } else {
            saved_idx % proxies.len()
        };
        let mtime = fs::metadata(&proxies_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let idx_mtime = fs::metadata(&idx_path).ok().and_then(|m| m.modified().ok());
        let (rotation_tx, _) = tokio::sync::broadcast::channel(32);

        Self {
            proxies: Arc::new(RwLock::new(proxies)),
            active_index: Arc::new(AtomicUsize::new(idx)),
            last_mtime: Arc::new(RwLock::new(mtime)),
            last_index_mtime: Arc::new(RwLock::new(idx_mtime)),
            rotation_tx,
            config_dir,
            last_fs_check_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn subscribe_rotation(&self) -> tokio::sync::broadcast::Receiver<usize> {
        self.rotation_tx.subscribe()
    }

    /// Return the platform configuration directory for oplire.
    pub fn config_dir() -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("oplire");
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(appdata) = std::env::var_os("APPDATA") {
                return PathBuf::from(appdata).join("oplire");
            }
            if let Some(profile) = std::env::var_os("USERPROFILE") {
                return PathBuf::from(profile).join(".config").join("oplire");
            }
        }
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg).join("oplire");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config").join("oplire");
        }
        // Never fall back to "." (VULN-07).
        PathBuf::from(".config").join("oplire")
    }

    pub fn config_proxies_path() -> PathBuf {
        Self::config_dir().join("proxies.txt")
    }

    fn proxies_path(&self) -> PathBuf {
        self.config_dir.join("proxies.txt")
    }

    fn index_file_path(&self) -> PathBuf {
        self.config_dir.join("active_proxy.idx")
    }

    async fn read_saved_index_async(&self) -> usize {
        let path = self.index_file_path();
        tokio::fs::read_to_string(path)
            .await
            .ok()
            .and_then(|content| content.trim().parse::<usize>().ok())
            .unwrap_or(0)
    }

    async fn save_index_async(&self, idx: usize) {
        let path = self.index_file_path();
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = tokio::fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).await;
            }
        }
        let _ = tokio::fs::write(path, idx.to_string()).await;
    }

    pub fn load_default() -> Self {
        // Priority 1: ~/.config/oplire/proxies.txt
        let config_file = Self::config_proxies_path();
        if config_file.exists() {
            if let Ok(pool) = Self::load_from_file(&config_file, "custom") {
                if !pool.is_empty_sync() {
                    return pool;
                }
            }
        }

        // Priority 2: proxiware-isp.txt in current workspace
        let local_proxiware = Path::new("proxiware-isp.txt");
        if local_proxiware.exists() {
            if let Ok(pool) = Self::load_from_file(local_proxiware, "proxiware-isp") {
                if !pool.is_empty_sync() {
                    let _ = pool.save_to_config();
                    return pool;
                }
            }
        }

        // Priority 3: ~/oplire-master/proxiware-isp.txt
        if let Ok(home) = std::env::var("HOME") {
            let home_proxiware = PathBuf::from(&home)
                .join("oplire-master")
                .join("proxiware-isp.txt");
            if home_proxiware.exists() {
                if let Ok(pool) = Self::load_from_file(&home_proxiware, "proxiware-isp") {
                    if !pool.is_empty_sync() {
                        let _ = pool.save_to_config();
                        return pool;
                    }
                }
            }
        }

        // Priority 4: Webshare-10-proxies.txt
        let local_webshare = Path::new("Webshare-10-proxies.txt");
        if local_webshare.exists() {
            if let Ok(pool) = Self::load_from_file(local_webshare, "webshare") {
                if !pool.is_empty_sync() {
                    return pool;
                }
            }
        }

        Self::default()
    }

    pub fn is_empty_sync(&self) -> bool {
        if let Ok(guard) = self.proxies.try_read() {
            guard.is_empty()
        } else {
            false
        }
    }

    pub fn load_from_file(path: &Path, default_provider: &str) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let mut list = Vec::new();
        let provider = if default_provider == "custom" {
            let fname = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            if fname.contains("proxiware") {
                "proxiware-isp"
            } else if fname.contains("webshare") {
                "webshare"
            } else {
                "custom"
            }
        } else {
            default_provider
        };

        for line in content.lines() {
            if let Some(entry) = ProxyEntry::parse(line, provider) {
                list.push(entry);
            }
        }

        Ok(Self::new(list))
    }

    pub fn save_to_config(&self) -> Result<()> {
        let path = self.proxies_path();
        let content = {
            let guard = self
                .proxies
                .try_read()
                .map_err(|_| anyhow!("Could not acquire read lock on proxies pool"))?;
            let mut lines = Vec::with_capacity(guard.len());
            for proxy in guard.iter() {
                lines.push(proxy.to_line());
            }
            lines.join("\n")
        };
        write_restricted_file(&path, &content)
    }

    pub async fn reload_if_modified(&self) {
        let now = now_millis();
        let last = self.last_fs_check_ms.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < FS_CHECK_INTERVAL_MS {
            return;
        }
        if self
            .last_fs_check_ms
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let proxies_path = self.proxies_path();
        if let Ok(meta) = tokio::fs::metadata(&proxies_path).await {
            if let Ok(current_mtime) = meta.modified() {
                let should_reload = {
                    let guard = self.last_mtime.read().await;
                    match *guard {
                        Some(last_mtime) => current_mtime > last_mtime,
                        None => true,
                    }
                };

                if should_reload {
                    if let Ok(content) = tokio::fs::read_to_string(&proxies_path).await {
                        let list: Vec<ProxyEntry> = content
                            .lines()
                            .filter_map(|line| ProxyEntry::parse(line, "custom"))
                            .collect();
                        if !list.is_empty() {
                            let new_len = list.len();
                            let saved_idx = self.read_saved_index_async().await;
                            let effective_idx = saved_idx % new_len;

                            {
                                let mut guard = self.proxies.write().await;
                                *guard = list;
                            }
                            {
                                let mut guard = self.last_mtime.write().await;
                                *guard = Some(current_mtime);
                            }

                            self.active_index.store(effective_idx, Ordering::SeqCst);
                            let _ = self.rotation_tx.send(effective_idx);
                            info!(
                                "Auto-reloaded {} proxies from {}",
                                new_len,
                                proxies_path.display()
                            );
                        }
                    }
                }
            }
        }

        let idx_path = self.index_file_path();
        if let Ok(meta) = tokio::fs::metadata(&idx_path).await {
            if let Ok(current_mtime) = meta.modified() {
                let should_reload_idx = {
                    let guard = self.last_index_mtime.read().await;
                    match *guard {
                        Some(last_mtime) => current_mtime > last_mtime,
                        None => true,
                    }
                };

                if should_reload_idx {
                    let saved_idx = self.read_saved_index_async().await;
                    let total = {
                        let guard = self.proxies.read().await;
                        guard.len()
                    };

                    if total > 0 {
                        let effective_idx = saved_idx % total;
                        let current_idx = self.active_index.load(Ordering::SeqCst) % total;
                        if current_idx != effective_idx {
                            self.active_index.store(effective_idx, Ordering::SeqCst);
                            let _ = self.rotation_tx.send(effective_idx);
                            info!(
                                "Active proxy index updated from disk to {}",
                                effective_idx + 1
                            );
                        }
                    }

                    {
                        let mut guard = self.last_index_mtime.write().await;
                        *guard = Some(current_mtime);
                    }
                }
            }
        }
    }

    pub async fn get_active(&self) -> Option<ProxyEntry> {
        self.reload_if_modified().await;

        let guard = self.proxies.read().await;
        if guard.is_empty() {
            return None;
        }
        let idx = self.active_index.load(Ordering::SeqCst) % guard.len();
        guard.get(idx).cloned()
    }

    pub async fn rotate(&self) -> Option<ProxyEntry> {
        self.reload_if_modified().await;

        let (next_idx, total, entry) = {
            let guard = self.proxies.read().await;
            if guard.len() <= 1 {
                if guard.is_empty() {
                    tracing::warn!("Cannot rotate: proxy pool is empty");
                } else {
                    tracing::warn!(
                        "Cannot rotate: proxy pool only has 1 proxy ({}:{})",
                        guard[0].host,
                        guard[0].port
                    );
                }
                return None;
            }
            let total = guard.len();
            let prev = self.active_index.fetch_add(1, Ordering::SeqCst);
            let next_idx = prev.wrapping_add(1) % total;
            (next_idx, total, guard.get(next_idx).cloned())
        };

        self.save_index_async(next_idx).await;

        if let Ok(meta) = tokio::fs::metadata(self.index_file_path()).await {
            if let Ok(mtime) = meta.modified() {
                let mut guard = self.last_index_mtime.write().await;
                *guard = Some(mtime);
            }
        }

        let _ = self.rotation_tx.send(next_idx);

        if let Some(ref proxy) = entry {
            info!(
                "Proxy rotated to index [{}/{}]: {} ({}:{})",
                next_idx + 1,
                total,
                proxy.provider,
                proxy.host,
                proxy.port
            );
        }
        entry
    }

    #[cfg(test)]
    fn disable_fs_reload_for_test(&self) {
        self.last_fs_check_ms.store(u64::MAX, Ordering::SeqCst);
    }

    pub async fn test_proxy(&self, entry: &ProxyEntry) -> Result<(String, Duration)> {
        let proxy_url = entry.url();
        let proxy = reqwest::Proxy::all(&proxy_url)?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(6))
            .build()?;

        let start = Instant::now();
        let resp = client.get("https://api.ipify.org").send().await?;
        let elapsed = start.elapsed();

        if resp.status().is_success() {
            let ip = resp.text().await?.trim().to_string();
            Ok((ip, elapsed))
        } else {
            Err(anyhow!("HTTP status: {}", resp.status()))
        }
    }

    pub async fn fetch_from_proxiware_api(api_key: &str) -> Result<Vec<ProxyEntry>> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()?;

        info!("Fetching Proxiware ISP subscriptions...");
        let subs_url = "https://api.proxiware.com/v1/static/networks/isp/subscriptions";
        let resp = client
            .get(subs_url)
            .header("X-API-Key", api_key)
            .send()
            .await
            .map_err(|e| {
                let message = e.to_string();
                let message = if api_key.is_empty() {
                    message
                } else {
                    message.replace(api_key, "***REDACTED***")
                };
                anyhow!("Proxiware request failed: {}", message)
            })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let body = if api_key.is_empty() {
                body
            } else {
                body.replace(api_key, "***REDACTED***")
            };
            return Err(anyhow!("Proxiware API error (status {}): {}", status, body));
        }

        let json: serde_json::Value = resp.json().await?;
        let mut proxies = Vec::new();

        if let Some(subscriptions) = json.get("data").and_then(|d| d.as_array()) {
            for sub in subscriptions {
                if let Some(sub_id) = sub.get("id").and_then(|i| i.as_str()) {
                    let proxies_url = format!(
                        "https://api.proxiware.com/v1/static/subscriptions/{}/proxies",
                        sub_id
                    );
                    if let Ok(proxy_resp) = client
                        .get(&proxies_url)
                        .header("X-API-Key", api_key)
                        .send()
                        .await
                    {
                        if let Ok(p_json) = proxy_resp.json::<serde_json::Value>().await {
                            if let Some(list) = p_json.get("data").and_then(|d| d.as_array()) {
                                for item in list {
                                    if let Some(line) = item.as_str() {
                                        if let Some(entry) =
                                            ProxyEntry::parse(line, "proxiware-api")
                                        {
                                            proxies.push(entry);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if proxies.is_empty() {
            Err(anyhow!(
                "No active static ISP proxies found on Proxiware account"
            ))
        } else {
            Ok(proxies)
        }
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn read_saved_index_from(path: &Path) -> usize {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

/// Write a file with restricted permissions (`0700` parent, `0600` file on Unix).
pub fn write_restricted_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(content.as_bytes())?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    #[cfg(not(unix))]
    fs::write(path, content)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_url_format() {
        let entry = ProxyEntry::parse("http://myuser:mypass@1.2.3.4:8080", "custom").unwrap();
        assert_eq!(entry.host, "1.2.3.4");
        assert_eq!(entry.port, 8080);
        assert_eq!(entry.username.as_deref(), Some("myuser"));
        assert_eq!(entry.password.as_deref(), Some("mypass"));
        assert_eq!(entry.protocol, "http");
        assert_eq!(entry.url(), "http://myuser:mypass@1.2.3.4:8080");

        let socks = ProxyEntry::parse("socks5://user:pass@127.0.0.1:1080", "custom").unwrap();
        assert_eq!(socks.protocol, "socks5");
        assert_eq!(socks.port, 1080);
    }

    #[test]
    fn test_parse_host_port_user_pass() {
        let entry =
            ProxyEntry::parse("188.220.163.167:1337:alice:secret", "proxiware-isp").unwrap();
        assert_eq!(entry.host, "188.220.163.167");
        assert_eq!(entry.port, 1337);
        assert_eq!(entry.username.as_deref(), Some("alice"));
        assert_eq!(entry.password.as_deref(), Some("secret"));
        assert_eq!(entry.provider, "proxiware-isp");
        assert_eq!(entry.protocol, "http");
    }

    #[test]
    fn test_parse_host_port_only() {
        let entry = ProxyEntry::parse("10.0.0.1:8000", "webshare").unwrap();
        assert_eq!(entry.host, "10.0.0.1");
        assert_eq!(entry.port, 8000);
        assert_eq!(entry.username, None);
        assert_eq!(entry.password, None);
        assert_eq!(entry.provider, "webshare");
    }

    #[test]
    fn test_parse_inline_comments() {
        let entry =
            ProxyEntry::parse("188.220.163.167:1337:user:pass # custom-tag", "default").unwrap();
        assert_eq!(entry.provider, "custom-tag");
        assert_eq!(entry.host, "188.220.163.167");
        assert_eq!(entry.port, 1337);
    }

    #[test]
    fn test_parse_comments_and_empty() {
        assert!(ProxyEntry::parse("", "custom").is_none());
        assert!(ProxyEntry::parse("   ", "custom").is_none());
        assert!(ProxyEntry::parse("# comment only", "custom").is_none());
        assert!(ProxyEntry::parse("invalid:too:many:colons:in:line", "custom").is_none());
    }

    #[test]
    fn test_parse_password_with_colons() {
        let entry = ProxyEntry::parse(
            "188.220.163.167:1337:alice:p@ss:w0rd:123 # proxiware-isp",
            "custom",
        )
        .expect("Should parse password containing colons");
        assert_eq!(entry.host, "188.220.163.167");
        assert_eq!(entry.port, 1337);
        assert_eq!(entry.username.as_deref(), Some("alice"));
        assert_eq!(entry.password.as_deref(), Some("p@ss:w0rd:123"));
        assert_eq!(entry.provider, "proxiware-isp");
    }

    #[test]
    fn test_debug_redacts_password() {
        let entry = ProxyEntry::parse("1.2.3.4:8080:user:supersecret", "custom").unwrap();
        let debug = format!("{entry:?}");
        assert!(!debug.contains("supersecret"));
        assert!(debug.contains("***REDACTED***"));
    }

    #[tokio::test]
    async fn test_pool_rotation_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = ProxyEntry::parse("1.1.1.1:8080", "custom").unwrap();
        let p2 = ProxyEntry::parse("2.2.2.2:8080", "custom").unwrap();
        let pool = ProxyPool::with_config_dir(vec![p1, p2], dir.path().join("oplire"));

        let mut rx = pool.subscribe_rotation();

        let rotated = pool.rotate().await;
        assert!(rotated.is_some());
        let new_idx = rx.recv().await.unwrap();
        assert_eq!(new_idx, pool.active_index.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_pool_rotation_single_proxy_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = ProxyEntry::parse("1.1.1.1:8080", "custom").unwrap();
        let pool = ProxyPool::with_config_dir(vec![p1], dir.path().join("oplire"));

        let rotated = pool.rotate().await;
        assert!(
            rotated.is_none(),
            "Pool with <= 1 proxy should return None on rotate to prevent no-op loops"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_rotations_consistency() {
        let dir = tempfile::tempdir().unwrap();
        let proxies: Vec<ProxyEntry> = (1..=5)
            .map(|i| ProxyEntry::parse(&format!("10.0.0.{i}:8080"), "custom").unwrap())
            .collect();
        let pool = ProxyPool::with_config_dir(proxies, dir.path().join("oplire"));
        pool.disable_fs_reload_for_test();
        pool.active_index.store(0, Ordering::SeqCst);

        let mut handles = Vec::new();
        for _ in 0..50 {
            let cloned = pool.clone();
            handles.push(tokio::spawn(async move { cloned.rotate().await }));
        }

        tokio::time::timeout(Duration::from_secs(10), async {
            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await
        .expect("concurrent rotations must not deadlock");

        assert_eq!(pool.active_index.load(Ordering::SeqCst) % 5, 0);
    }

    #[cfg(unix)]
    #[test]
    fn test_write_restricted_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("oplire");
        let path = nested.join("proxies.txt");
        write_restricted_file(&path, "1.1.1.1:8080:user:pass").unwrap();

        let dir_mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "1.1.1.1:8080:user:pass");
    }

    #[test]
    fn test_parse_domain_name_host() {
        let entry = ProxyEntry::parse(
            "ddc.oxylabs.io:8001:ddc_user:secret_pass # oxylabs",
            "custom",
        )
        .unwrap();
        assert_eq!(entry.host, "ddc.oxylabs.io");
        assert_eq!(entry.port, 8001);
        assert_eq!(entry.username.as_deref(), Some("ddc_user"));
        assert_eq!(entry.password.as_deref(), Some("secret_pass"));
        assert_eq!(entry.provider, "oxylabs");
    }
}
