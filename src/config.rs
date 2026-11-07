//! Application configuration definition and defaults.

/// Persistent configuration for the oplire proxy and watch service.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Socket address for the local forward proxy to listen on (e.g. `127.0.0.1:8080`).
    pub listen: String,
    /// Upstream OpenCode Zen base URL (e.g. `http://localhost:3000`).
    pub upstream: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".to_string(),
            upstream: "http://localhost:3000".to_string(),
        }
    }
}
