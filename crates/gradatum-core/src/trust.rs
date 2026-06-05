//! Trust context — caveat C10 (design spec P2.0a).
//!
//! Mandatory enum carried by every protected handler via Axum `Extension`.
//! No handler reads `Authorization` directly: extraction lives in
//! `gradatum-server::middleware::TrustExtractor`.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Identifie l'origine et le niveau de confiance d'une requête entrante.
///
/// Passé comme `Extension<TrustContext>` dans chaque handler Axum protégé.
/// L'extraction depuis les headers HTTP vit dans `gradatum-server::middleware`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TrustContext {
    /// Requête non authentifiée — accès refusé pour toute route protégée.
    Unauthenticated,
    /// JWT bearer token présenté via `Authorization: Bearer <token>`.
    BearerToken {
        /// Key ID (`kid` claim JWT) permettant la rotation de clé.
        kid: String,
        /// Audience attendue — doit être `"gradatum"` pour ce service.
        aud: String,
        /// Subject — identité du porteur (agent ID, user ID, etc.).
        sub: String,
        /// Scopes autorisés (`"read"`, `"write"`, `"service"`, ...).
        scopes: Vec<String>,
        /// Tenant cible — D10 multi-tenancy invariant (D3-complet, AUTH-T7, spec V2 2026-05-06).
        /// Valeur `"main"` pour le tenant racine (défaut alpha.5).
        tenant_id: String,
    },
    /// Client mTLS — certificat vérifié par la couche TLS.
    Mtls {
        /// Common Name extrait du certificat client.
        cn: String,
        /// Empreinte SHA-256 du certificat client (32 octets).
        fingerprint_sha256: [u8; 32],
    },
    /// Session Studio (UI admin) — authentification interactive.
    Studio {
        /// Email ou identifiant de l'utilisateur Studio.
        user: String,
        /// Scope de la session Studio.
        scope: StudioScope,
        /// Échéance d'une élévation de privilèges (`sudo`-like step-up).
        step_up_until: Option<SystemTime>,
    },
}

/// Niveaux d'accès pour une session Studio (UI admin).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StudioScope {
    /// Lecture seule — exploration, audit, recherche.
    ReadOnly,
    /// Opérateur — lecture + écriture, pas d'admin.
    Operator,
    /// Administrateur — accès complet incluant gestion des tokens.
    Admin,
}

impl TrustContext {
    /// Retourne `true` pour toute variante non-`Unauthenticated`.
    pub fn is_authenticated(&self) -> bool {
        !matches!(self, TrustContext::Unauthenticated)
    }

    /// Retourne `true` si le bearer token porte le scope `"service"`.
    ///
    /// Les agents (mcp-stub statique, service backends, etc.) reçoivent un JWT avec
    /// `scope: ["service"]` — éligibles au TTL long (tier R-A1).
    pub fn is_service_bearer(&self) -> bool {
        matches!(
            self,
            TrustContext::BearerToken { scopes, .. } if scopes.iter().any(|s| s == "service")
        )
    }

    /// Retourne le `tenant_id` si présent dans le contexte.
    ///
    /// Retourne `None` pour les variantes sans tenant (`Unauthenticated`, `Mtls`, `Studio`).
    /// D3-complet (AUTH-T7) : seul `BearerToken` porte le tenant.
    pub fn tenant_id(&self) -> Option<&str> {
        match self {
            TrustContext::BearerToken { tenant_id, .. } => Some(tenant_id.as_str()),
            _ => None,
        }
    }
}

/// Trait utilisé par le middleware pour extraire [`TrustContext`] depuis les
/// parties d'une requête HTTP.
///
/// Les implémentations concrètes vivent dans `gradatum-server::middleware`.
/// La séparation permet de tester l'extraction indépendamment des handlers.
#[async_trait::async_trait]
pub trait TrustExtractor: Send + Sync {
    /// Extrait le contexte de confiance depuis les headers/parties de la requête.
    ///
    /// # Erreurs
    ///
    /// Retourne [`TrustError::Missing`] si aucun credential n'est présent,
    /// [`TrustError::InvalidBearer`] si le JWT est malformé ou expiré,
    /// [`TrustError::InvalidMtls`] si le certificat client est invalide.
    async fn extract(&self, parts: &http::request::Parts) -> Result<TrustContext, TrustError>;
}

/// Erreurs possibles lors de l'extraction du contexte de confiance.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// Aucun credential présent dans la requête.
    #[error("missing authentication credentials")]
    Missing,
    /// Bearer token JWT invalide (malformé, expiré, signature incorrecte).
    #[error("invalid bearer token: {0}")]
    InvalidBearer(String),
    /// Certificat mTLS invalide ou chaîne de confiance non vérifiée.
    #[error("invalid mTLS certificate: {0}")]
    InvalidMtls(String),
}
