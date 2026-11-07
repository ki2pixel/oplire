//! Proxy pool management, format parsing, file persistence, and rotation mechanisms.

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::info;

/// Representation of a single proxy upstream destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

        // Support inline comments/tags: e.g. "188.220.163.167:1337:user:pass # proxiware-isp"
        let (clean_line, tag) = if let Some((before, after)) = line.split_once('#') {
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

        // Case 2: host:port:user:pass (Standard Proxiware & Webshare format)
        let parts: Vec<&str> = clean_line.split(':').collect();
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
    pub last_mtime: Arc<RwLock<Option<std::time::SystemTime>>>,
    /// Last recorded modification timestamp of active_proxy.idx
    pub last_index_mtime: Arc<RwLock<Option<std::time::SystemTime>>>,
    /// Broadcast channel transmitting index change events on rotation
    pub rotation_tx: tokio::sync::broadcast::Sender<usize>,
}

impl Default for ProxyPool {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ProxyPool {
    /// Create a new ProxyPool with initial proxies and load persistent active index if available.
    pub fn new(proxies: Vec<ProxyEntry>) -> Self {
        let saved_idx = Self::read_saved_index();
        let idx = if proxies.is_empty() {
            0
        } else {
            saved_idx % proxies.len()
        };
        let path = Self::config_proxies_path();
        let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        let idx_path = Self::index_file_path();
        let idx_mtime = fs::metadata(&idx_path).ok().and_then(|m| m.modified().ok());
        let (rotation_tx, _) = tokio::sync::broadcast::channel(32);

        Self {
            proxies: Arc::new(RwLock::new(proxies)),
            active_index: Arc::new(AtomicUsize::new(idx)),
            last_mtime: Arc::new(RwLock::new(mtime)),
            last_index_mtime: Arc::new(RwLock::new(idx_mtime)),
            rotation_tx,
        }
    }

    pub fn subscribe_rotation(&self) -> tokio::sync::broadcast::Receiver<usize> {
        self.rotation_tx.subscribe()
    }

    pub fn config_proxies_path() -> PathBuf {
        let mut path = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            PathBuf::from(xdg)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".config")
        } else {
            PathBuf::from(".")
        };
        path.push("oplire");
        let _ = fs::create_dir_all(&path);
        path.push("proxies.txt");
        path
    }

    fn index_file_path() -> PathBuf {
        let mut path = Self::config_proxies_path();
        path.set_file_name("active_proxy.idx");
        path
    }

    fn read_saved_index() -> usize {
        let path = Self::index_file_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(idx) = content.trim().parse::<usize>() {
                    return idx;
                }
            }
        }
        0
    }

    async fn read_saved_index_async() -> usize {
        let path = Self::index_file_path();
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            if let Ok(idx) = content.trim().parse::<usize>() {
                return idx;
            }
        }
        0
    }

    async fn save_index_async(idx: usize) {
        let path = Self::index_file_path();
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
        let path = Self::config_proxies_path();
        let guard = self
            .proxies
            .try_read()
            .map_err(|_| anyhow!("Could not acquire read lock on proxies pool"))?;
        let mut lines = Vec::new();
        for p in guard.iter() {
            lines.push(p.to_line());
        }
        fs::write(path, lines.join("\n"))?;
        Ok(())
    }

    pub async fn reload_if_modified(&self) {
        let path = Self::config_proxies_path();
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if let Ok(current_mtime) = meta.modified() {
                let should_reload = {
                    let guard = self.last_mtime.read().await;
                    match *guard {
                        Some(last) => current_mtime > last,
                        None => true,
                    }
                };

                if should_reload {
                    if let Ok(content) = tokio::fs::read_to_string(&path).await {
                        let mut list = Vec::new();
                        for line in content.lines() {
                            if let Some(entry) = ProxyEntry::parse(line, "custom") {
                                list.push(entry);
                            }
                        }
                        if !list.is_empty() {
                            let mut p_guard = self.proxies.write().await;
                            *p_guard = list;
                            let mut m_guard = self.last_mtime.write().await;
                            *m_guard = Some(current_mtime);
                            let saved_idx = Self::read_saved_index_async().await;
                            self.active_index.store(saved_idx, Ordering::SeqCst);
                            let _ = self.rotation_tx.send(saved_idx);
                            info!(
                                "Auto-reloaded {} proxies from {}",
                                p_guard.len(),
                                path.display()
                            );
                        }
                    }
                }
            }
        }

        let idx_path = Self::index_file_path();
        if let Ok(meta) = tokio::fs::metadata(&idx_path).await {
            if let Ok(current_mtime) = meta.modified() {
                let should_reload_idx = {
                    let guard = self.last_index_mtime.read().await;
                    match *guard {
                        Some(last) => current_mtime > last,
                        None => true,
                    }
                };

                if should_reload_idx {
                    let mut m_guard = self.last_index_mtime.write().await;
                    *m_guard = Some(current_mtime);
                    let saved_idx = Self::read_saved_index_async().await;
                    let current = self.active_index.load(Ordering::SeqCst);
                    if current != saved_idx {
                        self.active_index.store(saved_idx, Ordering::SeqCst);
                        let _ = self.rotation_tx.send(saved_idx);
                        info!("Active proxy index updated from disk to {}", saved_idx + 1);
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
        let idx = self.active_index.load(Ordering::Relaxed) % guard.len();
        guard.get(idx).cloned()
    }

    pub async fn rotate(&self) -> Option<ProxyEntry> {
        self.reload_if_modified().await;

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
        let current_idx = self.active_index.load(Ordering::SeqCst);
        let next_idx = (current_idx + 1) % total;
        self.active_index.store(next_idx, Ordering::SeqCst);
        Self::save_index_async(next_idx).await;

        // Broadcast rotation event to interrupt existing CONNECT tunnels
        let _ = self.rotation_tx.send(next_idx);

        let entry = guard.get(next_idx).cloned();
        if let Some(ref p) = entry {
            info!(
                "Proxy rotated to index [{}/{}]: {} ({}:{})",
                next_idx + 1,
                total,
                p.provider,
                p.host,
                p.port
            );
        }
        entry
    }

    pub async fn test_proxy(&self, entry: &ProxyEntry) -> Result<(String, Duration)> {
        let proxy_url = entry.url();
        let proxy = reqwest::Proxy::all(&proxy_url)?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
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
            .timeout(Duration::from_secs(10))
            .build()?;

        info!("Fetching Proxiware ISP subscriptions...");
        let subs_url = "https://api.proxiware.com/v1/static/networks/isp/subscriptions";
        let resp = client
            .get(subs_url)
            .header("X-API-Key", api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!(
                "Proxiware API error (status {}): {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
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

    #[tokio::test]
    async fn test_pool_rotation_broadcast() {
        let p1 = ProxyEntry::parse("1.1.1.1:8080", "custom").unwrap();
        let p2 = ProxyEntry::parse("2.2.2.2:8080", "custom").unwrap();
        let pool = ProxyPool::new(vec![p1, p2]);

        let mut rx = pool.subscribe_rotation();

        let rotated = pool.rotate().await;
        assert!(rotated.is_some());
        let new_idx = rx.recv().await.unwrap();
        assert_eq!(
            new_idx,
            pool.active_index.load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn test_pool_rotation_single_proxy_returns_none() {
        let p1 = ProxyEntry::parse("1.1.1.1:8080", "custom").unwrap();
        let pool = ProxyPool::new(vec![p1]);

        let rotated = pool.rotate().await;
        assert!(
            rotated.is_none(),
            "Pool with <= 1 proxy should return None on rotate to prevent no-op loops"
        );
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
