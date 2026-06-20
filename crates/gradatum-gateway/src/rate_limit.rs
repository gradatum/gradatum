//! Inbound per-IP rate limiter.
//!
//! Algorithm: sliding window per minute.
//!
//! `extract_client_ip_from_socket()` uses `ConnectInfo<SocketAddr>`
//! (real TCP address provided by the kernel) instead of `X-Forwarded-For` /
//! `X-Real-IP` headers, which are trivially spoofable by any remote client.
//!
//! `limit = 0` disables rate limiting entirely.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{ConnectInfo, Extension};

/// Per-IP counting window.
#[derive(Debug)]
struct Window {
    count: u32,
    started_at: Instant,
}

/// Shared per-IP rate limiter — thread-safe via `Arc<Mutex<...>>`.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    windows: Arc<Mutex<HashMap<IpAddr, Window>>>,
    max_per_minute: u32,
}

/// Extracts the client IP from the `ConnectInfo<SocketAddr>` extension (real TCP socket address).
///
/// Uses the real TCP socket address provided by the kernel via
/// `into_make_service_with_connect_info`. Unlike `X-Forwarded-For` / `X-Real-IP`
/// headers, this address cannot be forged by the client — it reflects the actual
/// TCP connection.
///
/// Axum 0.8 (axum-core 0.5): `Option<ConnectInfo<T>>` no longer satisfies
/// `FromRequestParts` directly. Use `Option<Extension<ConnectInfo<T>>>` instead,
/// which goes through `OptionalFromRequestParts` for `Extension<T>`.
///
/// Returns `127.0.0.1` as a fallback when the extension is absent (mocked transport
/// without a real TCP socket — test-only scenario).
pub fn extract_client_ip_from_socket(
    connect_info: &Option<Extension<ConnectInfo<SocketAddr>>>,
) -> IpAddr {
    connect_info
        .as_ref()
        .map(|Extension(ci)| ci.0.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

impl RateLimiter {
    /// Builds a rate limiter with the given limit.
    ///
    /// `max_per_minute = 0` disables rate limiting.
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            windows: Arc::new(Mutex::new(HashMap::new())),
            max_per_minute,
        }
    }

    /// Checks whether a request from the given IP is allowed.
    ///
    /// Returns `true` if allowed (and increments the counter).
    /// Returns `false` if the quota is exceeded.
    pub fn check_and_increment(&self, ip: IpAddr) -> bool {
        if self.max_per_minute == 0 {
            return true;
        }

        // Poison recovery : si un thread a paniqué en tenant ce lock, on récupère
        // l'état (potentiellement incohérent) au lieu de propager le panic. Un
        // rate-limiter tolère un compteur transitoirement faux bien mieux qu'un
        // auto-DoS où tous les appels suivants paniquent en cascade. L'invariant de
        // sécurité (plafonner les requêtes) reste assuré par le re-seed de fenêtre.
        let mut map = self
            .windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let now = Instant::now();
        let window_duration = std::time::Duration::from_secs(60);

        // Periodically evict expired entries.
        if map.len() > 1000 {
            map.retain(|_, w| w.started_at.elapsed() < window_duration);
        }

        let entry = map.entry(ip).or_insert_with(|| Window {
            count: 0,
            started_at: now,
        });

        if entry.started_at.elapsed() >= window_duration {
            entry.count = 0;
            entry.started_at = now;
        }

        if entry.count >= self.max_per_minute {
            false
        } else {
            entry.count += 1;
            true
        }
    }

    pub fn max_per_minute(&self) -> u32 {
        self.max_per_minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, a))
    }

    #[test]
    fn test_zero_limit_always_allows() {
        let rl = RateLimiter::new(0);
        for _ in 0..1000 {
            assert!(rl.check_and_increment(ip(1)));
        }
    }

    #[test]
    fn test_limit_enforced() {
        let rl = RateLimiter::new(3);
        assert!(rl.check_and_increment(ip(1)));
        assert!(rl.check_and_increment(ip(1)));
        assert!(rl.check_and_increment(ip(1)));
        assert!(!rl.check_and_increment(ip(1)));
    }

    #[test]
    fn test_different_ips_independent() {
        let rl = RateLimiter::new(2);
        assert!(rl.check_and_increment(ip(1)));
        assert!(rl.check_and_increment(ip(1)));
        assert!(!rl.check_and_increment(ip(1)));
        assert!(rl.check_and_increment(ip(2)));
    }

    #[test]
    fn test_window_reset_after_expiry() {
        use std::time::Duration;

        let rl = RateLimiter::new(2);
        {
            let mut map = rl.windows.lock().unwrap();
            map.insert(
                ip(1),
                Window {
                    count: 2,
                    started_at: Instant::now()
                        .checked_sub(Duration::from_secs(61))
                        .unwrap_or(Instant::now()),
                },
            );
        }
        assert!(rl.check_and_increment(ip(1)));
    }

    /// Vérifie que `extract_client_ip_from_socket` retourne l'IP socket réelle.
    #[test]
    fn test_extract_ip_from_socket_with_connect_info() {
        // 198.51.100.x = RFC 5737 TEST-NET-2 (documentation range)
        let addr: SocketAddr = "198.51.100.42:54321".parse().unwrap();
        let ci = Some(Extension(ConnectInfo(addr)));
        let ip = extract_client_ip_from_socket(&ci);
        assert_eq!(ip.to_string(), "198.51.100.42");
    }

    /// Vérifie le fallback localhost quand ConnectInfo est absent (transport mocké).
    #[test]
    fn test_extract_ip_from_socket_without_connect_info() {
        let ip = extract_client_ip_from_socket(&None);
        assert!(ip.is_loopback());
    }

    /// FIX 2 — un lock empoisonné (thread paniqué en le tenant) ne doit PAS provoquer
    /// un panic en cascade sur les appels suivants (anti auto-DoS). La récupération de
    /// poison réutilise l'état interne et le rate-limiter reste fonctionnel.
    #[test]
    fn test_poisoned_lock_does_not_cascade_panic() {
        let rl = RateLimiter::new(5);

        // Empoisonner le mutex : un thread acquiert le lock puis panique.
        let rl_clone = rl.clone();
        let handle = std::thread::spawn(move || {
            let _guard = rl_clone.windows.lock().expect("acquire lock in thread");
            panic!("empoisonnement volontaire du mutex pour le test");
        });
        // Le thread panique → le mutex est désormais empoisonné.
        assert!(handle.join().is_err(), "le thread doit avoir paniqué");

        // L'appel suivant doit fonctionner sans paniquer malgré le poison.
        let allowed = rl.check_and_increment(ip(7));
        assert!(allowed, "premier appel post-poison doit être autorisé");
        // Le plafond reste appliqué (sécurité préservée).
        for _ in 0..4 {
            rl.check_and_increment(ip(7));
        }
        assert!(
            !rl.check_and_increment(ip(7)),
            "le plafond doit toujours être appliqué après récupération de poison"
        );
    }
}
