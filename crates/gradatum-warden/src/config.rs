//! Warden L0 configuration.

use serde::{Deserialize, Serialize};

/// Warden L0 configuration — IP filter + rate limit + loopback bypass.
///
/// All fields have safe default values via [`Default`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WardenConfig {
    /// Enables or disables the warden. When `false`, all requests pass through without any check.
    pub enabled: bool,
    /// Maximum number of requests per minute per IP.
    pub rate_limit_per_minute: u32,
    /// Allowed burst size (initial tokens in the bucket).
    pub rate_limit_burst: u32,
    /// When `true`, loopback addresses (127.x.x.x, ::1) bypass rate limiting and IP filters
    /// entirely — the handler is called directly.
    pub bypass_loopback: bool,
    /// Allowed CIDRs. Empty = all IPs are allowed (except those in `ip_deny`).
    #[serde(default)]
    pub ip_allow: Vec<ipnet::IpNet>,
    /// Denied CIDRs. Evaluated after `ip_allow`. Match → 403.
    #[serde(default)]
    pub ip_deny: Vec<ipnet::IpNet>,
}

impl Default for WardenConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rate_limit_per_minute: 60,
            rate_limit_burst: 10,
            bypass_loopback: true,
            ip_allow: vec![],
            ip_deny: vec![],
        }
    }
}
