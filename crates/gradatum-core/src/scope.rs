//! Identifiants de scope pour les overrides distribués.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.8.
//!
//! ## Design
//!
//! Trois axes de scope couvrent l'ensemble des cas d'usage Phase 1+ :
//! - `Vault`   : override applicable à tous les porteurs d'un vault.
//! - `Locus`   : override restreint à un périmètre ACL sub-vault.
//! - `Bearer`  : override personnalisé pour un porteur spécifique (Phase 2+).
//!
//! Les newtype wrappers (`VaultId`, `LocusId`, `BearerId`) sont `#[serde(transparent)]` →
//! compatibles avec les valeurs `String` YAML/JSON du prédécesseur v1.x sans migration.
//!
//! ## Décision
//!
//! Q1/B1 brainstorming the maintainer 2026-05-03 — layout OpenDAL-friendly impose
//! que `VaultId` et `LocusId` soient des newtypes plutôt que des alias de type
//! afin de garantir la sécurité de type aux frontières entre couches.

use serde::{Deserialize, Serialize};

/// Identifiant de vault (alias UI : `vault`).
///
/// Multi-tenancy mandatory (invariant D10, C4). Équivalent au `tenant_id` SQLite.
/// `#[serde(transparent)]` → sérialisé/désérialisé comme `String` bare.
///
/// **Migration** : remplace `pub type VaultId = String` du Chunk A de T03 (frontmatter.rs).
/// Compatible YAML sans changement de format (transparent round-trip).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VaultId(pub String);

impl VaultId {
    /// Construit un `VaultId` depuis une string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Retourne la représentation string du vault id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VaultId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for VaultId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for VaultId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for VaultId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// Permet `assert_eq!(fm.vault_id, "main")` dans les tests existants.
impl PartialEq<&str> for VaultId {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl PartialEq<str> for VaultId {
    fn eq(&self, other: &str) -> bool {
        self.0.as_str() == other
    }
}

/// Identifiant de locus — périmètre ACL sub-vault.
///
/// Optionnel dans `Frontmatter` : `None` = scope vault root.
/// `#[serde(transparent)]` → sérialisé/désérialisé comme `String` bare.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocusId(pub String);

impl LocusId {
    /// Construit un `LocusId` depuis une string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Retourne la représentation string du locus id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LocusId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for LocusId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for LocusId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for LocusId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Identifiant de porteur (bearer) — Phase 2+.
///
/// Utilisé pour les overrides personnalisés par utilisateur/agent.
/// Phase 1 = stub utilisé uniquement dans `OverrideScope::Bearer` et `AclPolicy`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BearerId(pub String);

impl BearerId {
    /// Construit un `BearerId` depuis une string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Retourne la représentation string du bearer id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BearerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BearerId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for BearerId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Scope d'un override distribué.
///
/// Détermine à quel périmètre s'applique un override dans la table `note_overrides`.
/// Décision Q7/B20 brainstorming the maintainer 2026-05-03 — 1 override actif par `(note, scope, type)`.
///
/// Sérialisé avec `#[serde(tag = "kind", content = "id")]` → JSON/TOML lisible :
/// `{ "kind": "vault", "id": "main" }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum OverrideScope {
    /// Override applicable à l'ensemble du vault.
    Vault(VaultId),
    /// Override restreint à un périmètre ACL sub-vault.
    Locus(LocusId),
    /// Override personnalisé pour un porteur spécifique (Phase 2+).
    Bearer(BearerId),
}
