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

/// Maximum length of a `VaultId` in bytes (DoS protection cap).
pub const VAULT_ID_MAX_LEN: usize = 64;

/// Vault identifier (UI alias: `vault`).
///
/// Multi-tenancy mandatory — equivalent to the `tenant_id` SQLite column.
/// `#[serde(transparent)]` → serialised/deserialised as a bare `String`.
///
/// Replaces a plain `pub type VaultId = String` while preserving YAML
/// round-trip compatibility (transparent serialisation).
///
/// ## Construction
///
/// - [`VaultId::new`] and the [`From`] impls are **not validated**: they accept any string.
///   They exist for internal use where the value is already trusted (migrations, tests,
///   SQLite row reconstruction). **Do not use at external input boundaries.**
/// - [`VaultId::parse`] is the validated constructor: use it whenever the `vault_id`
///   comes from untrusted input (HTTP request, CLI argument, YAML deserialization).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VaultId(String);

impl VaultId {
    /// Constructs a `VaultId` from a string **without validation**.
    ///
    /// Intended for internal use where the value is already trusted (migrations,
    /// tests, SQLite row reconstruction). Use [`VaultId::parse`] at external input
    /// boundaries.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation of the vault id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse-don't-validate: constructs a `VaultId` with strict validation.
    ///
    /// Rules (any violation → `ValidationError::InvalidVaultId`):
    /// - non-empty;
    /// - length ≤ [`VAULT_ID_MAX_LEN`] bytes (DoS protection cap);
    /// - restricted charset: `[a-z0-9-]` only (lowercase, digits, hyphens);
    /// - no leading or trailing hyphen.
    ///
    /// ## When to use
    ///
    /// Use `parse` at **untrusted input boundaries** where the vault_id comes from
    /// user input or an external source. The primary such boundary in the gradatum stack is
    /// the `gradatum-admin` CLI (argument `--tenant`). On the HTTP server path, the
    /// tenant identity is derived from the JWT claim (`sub`), which is validated and
    /// trusted by the time it reaches application code — JWT validation is the effective
    /// enforcement point, so server handlers use [`VaultId::new`] on the already-validated
    /// JWT tenant string rather than calling `parse` again.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::ValidationError::InvalidVaultId`] with an explanatory message.
    pub fn parse(s: &str) -> Result<Self, crate::error::ValidationError> {
        use crate::error::ValidationError;

        if s.is_empty() {
            return Err(ValidationError::InvalidVaultId("vault_id vide".to_string()));
        }
        if s.len() > VAULT_ID_MAX_LEN {
            return Err(ValidationError::InvalidVaultId(format!(
                "vault_id trop long ({} > {VAULT_ID_MAX_LEN} octets)",
                s.len()
            )));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(ValidationError::InvalidVaultId(format!(
                "charset interdit dans {s:?} (autorisé : a-z 0-9 -)"
            )));
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(ValidationError::InvalidVaultId(format!(
                "tiret en tête ou en queue interdit dans {s:?}"
            )));
        }
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for VaultId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Constructs a `VaultId` from an owned `String` **without validation**.
/// Use [`VaultId::parse`] at external input boundaries.
impl From<String> for VaultId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Constructs a `VaultId` from a `&str` **without validation**.
/// Use [`VaultId::parse`] at external input boundaries.
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
///
/// ## Construction
///
/// - [`LocusId::new`] and the [`From`] impls are **not validated**: they accept any string.
///   They exist for internal use where the value is already trusted. **Do not use at
///   external input boundaries.**
/// - [`LocusId::parse`] is the exclusive validated constructor: use it whenever the
///   `locus_id` comes from untrusted input (HTTP request, CLI argument, YAML deserialization).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocusId(String);

impl LocusId {
    /// Constructs a `LocusId` from a string **without validation**.
    ///
    /// Intended for internal use where the value is already trusted. Use
    /// [`LocusId::parse`] at external input boundaries.
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
pub struct BearerId(String);

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
mod vault_id_parse_tests {
    use super::*;
    use crate::error::ValidationError;

    /// `VaultId::parse` accepte les identifiants valides.
    #[test]
    fn vault_id_parse_accepts_valid() {
        for ok in ["main", "a", "my-vault", "vault01", "v0-prod", "abc123"] {
            assert!(
                VaultId::parse(ok).is_ok(),
                "{ok:?} doit être accepté par VaultId::parse"
            );
        }
    }

    /// `VaultId::parse` rejette vide, charset interdit, tiret tête/queue, trop long.
    #[test]
    fn vault_id_parse_rejects_invalid() {
        let bad = [
            "",         // vide
            "Main",     // majuscule
            "my vault", // espace
            "my_vault", // underscore
            "my/vault", // slash
            "café",     // non-ascii
            "-main",    // tiret initial
            "main-",    // tiret terminal
        ];
        for b in bad {
            assert!(
                VaultId::parse(b).is_err(),
                "{b:?} doit être rejeté par VaultId::parse"
            );
        }
        // Trop long (> VAULT_ID_MAX_LEN).
        let long = "a".repeat(VAULT_ID_MAX_LEN + 1);
        assert!(
            VaultId::parse(&long).is_err(),
            "vault_id > max doit être rejeté"
        );
        // Exactement la borne → accepté.
        let at_limit = "a".repeat(VAULT_ID_MAX_LEN);
        assert!(
            VaultId::parse(&at_limit).is_ok(),
            "vault_id == max doit être accepté"
        );
    }

    /// `VaultId::parse` produit `ValidationError::InvalidVaultId` (jamais une autre variante).
    #[test]
    fn vault_id_parse_produces_correct_error_variant() {
        let err = VaultId::parse("").unwrap_err();
        assert!(
            matches!(err, ValidationError::InvalidVaultId(_)),
            "attendu InvalidVaultId, obtenu {:?}",
            err
        );

        let err = VaultId::parse("Bad-Vault!").unwrap_err();
        assert!(
            matches!(err, ValidationError::InvalidVaultId(_)),
            "attendu InvalidVaultId, obtenu {:?}",
            err
        );
    }
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
