//! Per-IP rate limiter backed by a governor token bucket.

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};

use crate::error::WardenError;

/// Per-IP rate limiter: token bucket shared across all requests.
///
/// Constructed via [`PerIpRateLimiter::new`].
/// Thread-safe: inner `Arc`, cheap to clone.
#[derive(Clone)]
pub struct PerIpRateLimiter {
    inner: Arc<RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>>,
}

impl PerIpRateLimiter {
    /// Creates a rate limiter with `per_minute` tokens/min and `burst` initial tokens.
    ///
    /// # Errors
    ///
    /// Returns [`WardenError::InvalidRateLimit`] if `per_minute == 0` or `burst == 0`.
    pub fn new(per_minute: u32, burst: u32) -> Result<Self, WardenError> {
        if per_minute == 0 || burst == 0 {
            return Err(WardenError::InvalidRateLimit(per_minute, burst));
        }

        // Quota : 1 jeton toutes les (60 / per_minute) secondes, burst max.
        let refill_interval = Duration::from_secs_f64(60.0 / per_minute as f64);
        // NonZeroU32 : burst est garanti > 0 ici.
        let burst_nz = NonZeroU32::new(burst)
            .expect("burst > 0 vérifié ci-dessus — NonZeroU32::new ne peut pas échouer");

        let quota = Quota::with_period(refill_interval)
            .expect("refill_interval > 0 — per_minute > 0 garanti")
            .allow_burst(burst_nz);

        let limiter = RateLimiter::dashmap(quota);
        Ok(Self {
            inner: Arc::new(limiter),
        })
    }

    /// Checks whether a request from `ip` is allowed.
    ///
    /// Returns `true` if allowed (token available), `false` if the quota is exceeded.
    /// Consumes one token on success.
    pub fn check(&self, ip: IpAddr) -> bool {
        self.inner.check_key(&ip).is_ok()
    }

    /// Computes the minimum wait time in seconds before the next allowed request.
    ///
    /// Used to populate the `retry-after` header.
    /// Returns `1` when the quota is not exceeded (nominal case; this method should not be called then).
    pub fn wait_time_secs(&self, ip: IpAddr) -> u64 {
        match self.inner.check_key(&ip) {
            Ok(_) => 0,
            Err(not_until) => {
                let wait = not_until.wait_time_from(governor::clock::DefaultClock::default().now());
                wait.as_secs().max(1)
            }
        }
    }
}

impl std::fmt::Debug for PerIpRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PerIpRateLimiter").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_config_returns_error() {
        assert!(PerIpRateLimiter::new(0, 10).is_err());
        assert!(PerIpRateLimiter::new(60, 0).is_err());
    }

    #[test]
    fn burst_within_limit_all_pass() {
        let limiter = PerIpRateLimiter::new(60, 5).expect("config valide");
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        for _ in 0..5 {
            assert!(
                limiter.check(ip),
                "les 5 premiers jetons doivent être accordés"
            );
        }
    }

    #[test]
    fn burst_exceeded_blocked() {
        let limiter = PerIpRateLimiter::new(60, 5).expect("config valide");
        let ip: IpAddr = "192.0.2.2".parse().unwrap();
        // Vider le burst.
        for _ in 0..5 {
            limiter.check(ip);
        }
        assert!(
            !limiter.check(ip),
            "le 6e jeton doit être refusé (burst épuisé)"
        );
    }

    #[test]
    fn different_ips_independent_buckets() {
        let limiter = PerIpRateLimiter::new(60, 2).expect("config valide");
        let ip1: IpAddr = "192.0.2.10".parse().unwrap();
        let ip2: IpAddr = "192.0.2.11".parse().unwrap();
        // Vider ip1.
        limiter.check(ip1);
        limiter.check(ip1);
        assert!(!limiter.check(ip1), "ip1 épuisée");
        // ip2 doit avoir son propre bucket intact.
        assert!(limiter.check(ip2), "ip2 doit être indépendante de ip1");
    }
}
