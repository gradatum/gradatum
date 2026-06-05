//! Rate limiter inbound par IP — F-MAJ-3 fix.
//!
//! Algorithme : fenêtre glissante par minute.
//!
//! F-MAJ-3 fix : `extract_client_ip_from_socket()` utilise `ConnectInfo<SocketAddr>`
//! (adresse TCP réelle fournie par le kernel) au lieu des headers X-Forwarded-For /
//! X-Real-IP qui sont trivialement spoofables par tout client distant.
//!
//! `limit = 0` désactive entièrement le rate limiting.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::{ConnectInfo, Extension};

/// Fenêtre de comptage pour une IP.
#[derive(Debug)]
struct Window {
    count: u32,
    started_at: Instant,
}

/// Rate limiter partagé par IP — thread-safe via `Arc<Mutex<...>>`.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    windows: Arc<Mutex<HashMap<IpAddr, Window>>>,
    max_per_minute: u32,
}

/// Extrait l'IP cliente depuis l'extension `ConnectInfo<SocketAddr>` (F-MAJ-3 fix).
///
/// Utilise l'adresse TCP socket réelle fournie par le kernel via
/// `into_make_service_with_connect_info`. Contrairement aux headers
/// X-Forwarded-For / X-Real-IP, cette adresse ne peut pas être falsifiée par
/// le client — elle reflète la connexion TCP réelle.
///
/// Axum 0.8 (axum-core 0.5) : `Option<ConnectInfo<T>>` ne satisfait plus
/// directement `FromRequestParts`. Utiliser `Option<Extension<ConnectInfo<T>>>`
/// qui passe via `OptionalFromRequestParts` pour `Extension<T>`.
///
/// Si l'extension est absente (transport mocké sans vrai socket TCP),
/// retourne `127.0.0.1` par défaut (cas test uniquement).
pub fn extract_client_ip_from_socket(
    connect_info: &Option<Extension<ConnectInfo<SocketAddr>>>,
) -> IpAddr {
    connect_info
        .as_ref()
        .map(|Extension(ci)| ci.0.ip())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

impl RateLimiter {
    /// Construit un rate limiter avec la limite donnée.
    ///
    /// `max_per_minute = 0` désactive le rate limiting.
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            windows: Arc::new(Mutex::new(HashMap::new())),
            max_per_minute,
        }
    }

    /// Vérifie si une requête depuis l'IP donnée est autorisée.
    ///
    /// Retourne `true` si autorisé (et incrémente le compteur).
    /// Retourne `false` si le quota est dépassé.
    pub fn check_and_increment(&self, ip: IpAddr) -> bool {
        if self.max_per_minute == 0 {
            return true;
        }

        let mut map = self
            .windows
            .lock()
            .expect("rate limiter mutex poisoned — process should restart");

        let now = Instant::now();
        let window_duration = std::time::Duration::from_secs(60);

        // Nettoyage périodique des entrées expirées.
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

    /// F-MAJ-3 : extract_client_ip_from_socket retourne l'IP socket réelle.
    #[test]
    fn test_extract_ip_from_socket_with_connect_info() {
        // 198.51.100.x = RFC 5737 TEST-NET-2 (documentation range)
        let addr: SocketAddr = "198.51.100.42:54321".parse().unwrap();
        let ci = Some(Extension(ConnectInfo(addr)));
        let ip = extract_client_ip_from_socket(&ci);
        assert_eq!(ip.to_string(), "198.51.100.42");
    }

    /// F-MAJ-3 : sans ConnectInfo (transport mocké), fallback localhost.
    #[test]
    fn test_extract_ip_from_socket_without_connect_info() {
        let ip = extract_client_ip_from_socket(&None);
        assert!(ip.is_loopback());
    }
}
