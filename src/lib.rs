//! # oplire
//!
//! OpenCode Proxy Pool Manager & Rate-Limit Rotation Library.
//!
//! This library provides the core modules for:
//! - [`config`]: Application configuration management (`config.json`).
//! - [`pool`]: Residential and ISP proxy pool loading, rotation, testing, and persistence.
//! - [`proxy`]: Local HTTP CONNECT forward proxy server with Basic Auth forwarding.
//! - [`watch`]: Real-time OpenCode log monitoring and automatic rotation upon rate-limit detection.
//! - [`audit`]: OpenCode transcript parsing, empirical quota cutoff analysis, and pool estimation.

pub mod audit;
pub mod config;
pub mod pool;
pub mod proxy;
pub mod watch;

pub use audit::{
    analyze_transcript, estimate_pool_size, parse_transcript_from_file, parse_transcript_from_str,
    print_audit_report, print_pool_estimation, AuditReport, PoolEstimation, ProxyCycle,
    SessionExport, NOMINAL_OUTPUT_QUOTA_PER_IP,
};
pub use config::AppConfig;
pub use pool::{write_restricted_file, ProxyEntry, ProxyPool};
