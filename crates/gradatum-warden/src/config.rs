//! Configuration du warden L0.

use serde::{Deserialize, Serialize};

/// Configuration du warden L0 — IP filter + rate limit + bypass loopback.
///
/// Tous les champs ont des valeurs par défaut sûres via [`Default`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WardenConfig {
    /// Active ou désactive le warden. Si `false`, toutes les requêtes passent sans contrôle.
    pub enabled: bool,
    /// Nombre maximal de requêtes par minute par IP.
    pub rate_limit_per_minute: u32,
    /// Taille du burst autorisé (jetons initiaux dans le seau).
    pub rate_limit_burst: u32,
    /// Si `true`, les adresses loopback (127.x.x.x, ::1) contournent intégralement
    /// le rate limit et les filtres IP — le handler métier est appelé directement.
    pub bypass_loopback: bool,
    /// CIDRs autorisés. Vide = toutes les IPs autorisées (sauf celles dans `ip_deny`).
    #[serde(default)]
    pub ip_allow: Vec<ipnet::IpNet>,
    /// CIDRs refusés. Évalués après `ip_allow`. Match → 403.
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
