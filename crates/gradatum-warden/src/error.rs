//! Warden error types and decision enum.

/// Construction error for the warden.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WardenError {
    /// Invalid rate limit configuration: `per_minute` or `burst` is zero.
    #[error("invalid rate limit: per_minute={0} burst={1} (both must be > 0)")]
    InvalidRateLimit(u32, u32),
}

/// Warden decision for an incoming request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WardenDecision {
    /// Request is allowed — passed to the next handler.
    Allow,
    /// Loopback bypass — forwarded directly to the handler, skipping rate limiting and IP filters.
    Bypass,
    /// IP denied by the CIDR filter → 403 Forbidden.
    DenyIp,
    /// Rate limit exceeded → 429 Too Many Requests.
    DenyRateLimit,
}
