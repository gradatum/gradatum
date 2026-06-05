//! # gradatum-auth
//!
//! Vérification d'identité externe : JWT (Ed25519, audience-scoped), OIDC, API-key.
//!
//! ## Stabilité
//!
//! `0.x` — aucune garantie de stabilité API. Tous les traits publics sont annotés
//! [`#[stability::unstable]`] ou [`#[stability::experimental]`].
//! Voir la [politique de versioning](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Statut
//!
//! Phase 2.0a — RevocationStore implémenté (T4). JWT Ed25519 implémenté (T5).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Version de la crate (héritée du `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod jwt;
pub mod revocation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
