//! Scope identifiers for distributed overrides.
//!
//! ## Design
//!
//! Three scope axes cover all use cases:
//! - `Vault`  : override applicable to all bearers of a vault.
//! - `Locus`  : override restricted to a sub-vault ACL scope.
//! - `Bearer` : override personalised for a specific bearer (requires `gradatum-acl-auth`).
//!
//! Newtype wrappers (`VaultId`, `LocusId`, `BearerId`) are `#[serde(transparent)]` →
//! compatible with `String` YAML/JSON values from predecessor v1.x without migration.
//!
//! ## Design decision
//!
//! OpenDAL-friendly layout requires `VaultId` and `LocusId` to be newtypes rather than
//! type aliases to guarantee type safety at inter-layer boundaries.

use serde::{Deserialize, Serialize};

/// Vault identifier (UI alias: `vault`).
///
/// Multi-tenancy mandatory — equivalent to the `tenant_id` SQLite column.
/// `#[serde(transparent)]` → serialised/deserialised as a bare `String`.
///
/// Replaces a plain `pub type VaultId = String` while preserving YAML
/// round-trip compatibility (transparent serialisation).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VaultId(pub String);

impl VaultId {
    /// Constructs a `VaultId` from a string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the vault id.
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

// Allows `assert_eq!(fm.vault_id, "main")` in existing tests.
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

/// Locus identifier — sub-vault ACL scope.
///
/// Optional in `Frontmatter`: `None` = vault root scope.
/// `#[serde(transparent)]` → serialised/deserialised as a bare `String`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocusId(pub String);

impl LocusId {
    /// Constructs a `LocusId` from a string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the locus id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse-don't-validate: constructs a `LocusId` with strict validation.
    ///
    /// Rules (any violation → `ValidationError::InvalidLocusId`):
    /// - non-empty;
    /// - length ≤ [`LOCUS_MAX_LEN`] bytes (DoS protection cap);
    /// - restricted charset: `[a-z0-9-/]` only (lowercase, digits, hyphens, slashes);
    /// - **anti-traversal**: no `..`, no leading/trailing slash, no `//`.
    ///
    /// Guarantees that an accepted locus is a safe logical path (no directory traversal,
    /// no empty segments), usable as an ACL prefix and as a physical path without
    /// additional escaping.
    ///
    /// # Errors
    /// Returns `ValidationError::InvalidLocusId` with an explanatory message.
    pub fn parse(s: &str) -> Result<Self, crate::error::ValidationError> {
        use crate::error::ValidationError;

        if s.is_empty() {
            return Err(ValidationError::InvalidLocusId("locus vide".to_string()));
        }
        if s.len() > LOCUS_MAX_LEN {
            return Err(ValidationError::InvalidLocusId(format!(
                "locus trop long ({} > {LOCUS_MAX_LEN} octets)",
                s.len()
            )));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '/')
        {
            return Err(ValidationError::InvalidLocusId(format!(
                "charset interdit dans {s:?} (autorisé : a-z 0-9 - /)"
            )));
        }
        // Anti-traversal : pas de remontée, pas de segment vide, pas de slash terminal/initial.
        if s.contains("..") {
            return Err(ValidationError::InvalidLocusId(format!(
                "remontée de répertoire interdite dans {s:?}"
            )));
        }
        if s.starts_with('/') || s.ends_with('/') || s.contains("//") {
            return Err(ValidationError::InvalidLocusId(format!(
                "slash en tête/queue ou segment vide interdit dans {s:?}"
            )));
        }
        Ok(Self(s.to_string()))
    }
}

/// Maximum length of a `LocusId` in bytes (DoS protection cap).
pub const LOCUS_MAX_LEN: usize = 128;

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

/// Bearer identifier.
///
/// Used for per-user/per-agent personalised overrides.
/// Consumed by `OverrideScope::Bearer` and `AclPolicy`; full implementation via `gradatum-acl-auth`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BearerId(pub String);

impl BearerId {
    /// Constructs a `BearerId` from a string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the bearer id.
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

/// Scope of a distributed override.
///
/// Determines the perimeter to which an override applies in the `note_overrides` table.
/// One active override per `(note, scope, type)`.
///
/// Serialised with `#[serde(tag = "kind", content = "id")]` → readable JSON/TOML:
/// `{ "kind": "vault", "id": "main" }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum OverrideScope {
    /// Override applicable to the entire vault.
    Vault(VaultId),
    /// Override restricted to a sub-vault ACL scope.
    Locus(LocusId),
    /// Override personalised for a specific bearer (requires `gradatum-acl-auth`).
    Bearer(BearerId),
}

#[cfg(test)]
mod locus_parse_tests {
    use super::*;

    /// F-37 S1.4 — `LocusId::parse` accepte les locus valides.
    #[test]
    fn locus_parse_accepts_valid() {
        for ok in ["knowledge", "knowledge/rust", "a", "a-b/c-d/e9", "x/y/z"] {
            assert!(LocusId::parse(ok).is_ok(), "{ok:?} doit être accepté");
        }
    }

    /// F-37 S1.4 — rejette vide, charset interdit, traversal, slash terminal, trop long.
    #[test]
    fn locus_parse_rejects_invalid() {
        let bad = [
            "",               // vide
            "Knowledge",      // majuscule
            "knowledge rust", // espace
            "know_ledge",     // underscore
            "../etc",         // traversal
            "a/../b",         // traversal interne
            "/knowledge",     // slash initial
            "knowledge/",     // slash terminal
            "a//b",           // segment vide
            "café",           // non-ascii
        ];
        for b in bad {
            assert!(LocusId::parse(b).is_err(), "{b:?} doit être rejeté");
        }
        // Trop long (> LOCUS_MAX_LEN).
        let long = "a".repeat(LOCUS_MAX_LEN + 1);
        assert!(
            LocusId::parse(&long).is_err(),
            "locus > max doit être rejeté"
        );
        // Exactement la borne → accepté.
        let at_limit = "a".repeat(LOCUS_MAX_LEN);
        assert!(
            LocusId::parse(&at_limit).is_ok(),
            "locus == max doit être accepté"
        );
    }
}
