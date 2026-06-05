//! Filtrage IP par CIDR.

use std::net::IpAddr;

/// Détermine si une adresse IP est autorisée selon les listes `allow` et `deny`.
///
/// Règles :
/// 1. Si `allow` est non vide et que l'IP n'est dans aucun CIDR allow → refusée.
/// 2. Si l'IP est dans un CIDR deny → refusée.
/// 3. Sinon → autorisée.
///
/// L'évaluation est : `allow_match && !deny_match`.
/// - `allow` vide = tout le monde est dans la liste blanche (par défaut).
/// - `deny` vide = personne n'est explicitement refusé.
pub fn ip_allowed(ip: IpAddr, allow: &[ipnet::IpNet], deny: &[ipnet::IpNet]) -> bool {
    // Si allow non vide et IP absent → refus.
    if !allow.is_empty() && !allow.iter().any(|net| net.contains(&ip)) {
        return false;
    }
    // Si IP dans deny → refus.
    if deny.iter().any(|net| net.contains(&ip)) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }
    fn net(s: &str) -> ipnet::IpNet {
        s.parse().unwrap()
    }

    #[test]
    fn empty_allow_empty_deny_permits_all() {
        // 198.51.100.x = RFC 5737 TEST-NET-2 (documentation range)
        assert!(ip_allowed(ip("10.0.0.1"), &[], &[]));
        assert!(ip_allowed(ip("198.51.100.1"), &[], &[]));
    }

    #[test]
    fn deny_cidr_blocks_ip() {
        let deny = vec![net("10.0.0.0/8")];
        assert!(!ip_allowed(ip("10.0.0.1"), &[], &deny));
        assert!(ip_allowed(ip("198.51.100.1"), &[], &deny));
    }

    #[test]
    fn allow_cidr_restricts_to_subnet() {
        let allow = vec![net("10.0.0.0/8")];
        assert!(ip_allowed(ip("10.0.0.1"), &allow, &[]));
        assert!(!ip_allowed(ip("198.51.100.1"), &allow, &[]));
    }

    #[test]
    fn deny_takes_precedence_over_allow() {
        let allow = vec![net("10.0.0.0/8")];
        let deny = vec![net("10.0.0.0/24")];
        // 10.0.0.1 est dans allow ET dans deny → refusé.
        assert!(!ip_allowed(ip("10.0.0.1"), &allow, &deny));
        // 10.1.0.1 est dans allow mais pas dans deny → autorisé.
        assert!(ip_allowed(ip("10.1.0.1"), &allow, &deny));
    }
}
