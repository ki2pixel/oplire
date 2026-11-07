//! # oplire
//!
//! OpenCode Proxy Pool Manager & Rate-Limit Rotation Library.
//!
//! This library provides the core modules for:
//! - [`config`]: Application configuration management (`config.json`).
//! - [`pool`]: Residential and ISP proxy pool loading, rotation, testing, and persistence.
//! - [`proxy`]: Local HTTP CONNECT forward proxy server with Basic Auth forwarding.
//! - [`watch`]: Real-time OpenCode log monitoring and automatic rotation upon rate-limit detection.

pub mod config;
pub mod pool;
pub mod proxy;
pub mod watch;

pub use config::AppConfig;
pub use pool::{write_restricted_file, ProxyEntry, ProxyPool};
