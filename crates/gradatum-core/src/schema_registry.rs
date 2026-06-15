//! Override schema registry — embedded via `include_dir!`.
//!
//! ## Design
//!
//! TOML schemas are embedded into the binary at compile time (`include_dir!`).
//! No filesystem access at runtime → no dependency on the working directory.
//!
//! Structure: `schemas/overrides/<override_type>-v<schema_version>.toml`
//!
//! 4 embedded schemas:
//! - `metadata-v1.toml` — stable, owner `gradatum-vault`
//! - `acl-v1.toml`      — experimental, owner `gradatum-acl-policy`
//! - `index-v1.toml`    — experimental, owner `gradatum-index`
//! - `score-v1.toml`    — experimental, owner `gradatum-curator`
//!
//! ## Validation and migration (stubs)
//!
//! `validate_payload` and `migrate_payload` always return `Ok`.
//! Full implementation (required fields, types, constraints) is planned for a future release.
//!
//! ## Design decision
//!
//! TOML-embedded schema registry — no external registry, no network access at runtime.

use std::collections::BTreeMap;

use include_dir::{include_dir, Dir};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Embedded schemas directory — relative to `CARGO_MANIFEST_DIR`.
static SCHEMAS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/schemas/overrides");

/// Override schema — deserialised from the embedded TOML files.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OverrideSchema {
    /// Override type discriminant (e.g. `"metadata"`).
    pub override_type: String,

    /// Schema version.
    pub schema_version: u32,

    /// Crate that owns the schema.
    pub owner_crate: String,

    /// Gradatum phase in which this schema is used.
    pub phase: u8,

    /// Schema stability status.
    pub status: SchemaStatus,

    /// Field specifications for the payload.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldSpec>,

    /// Migration directives from a previous version.
    #[serde(default)]
    pub migration: BTreeMap<String, Vec<MigrationStep>>,
}

/// Stability status of an override schema.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SchemaStatus {
    /// Stable schema — API guaranteed for the current phase.
    Stable,
    /// Schema in the process of stabilisation.
    Unstable,
    /// Experimental schema — may change without notice.
    Experimental,
    /// Deprecated schema — will be removed in the next major version.
    Deprecated,
}

/// Field specification in an override payload.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FieldSpec {
    /// Field type (e.g. `"enum:Section"`, `"array:Tag"`, `"string"`, `"AuthorRef"`).
    #[serde(rename = "type")]
    pub field_type: String,

    /// Whether the field is required in the payload.
    #[serde(default)]
    pub required: bool,

    /// Default value when the field is absent.
    #[serde(default)]
    pub default: Option<toml::Value>,

    /// Maximum length for `"string"` fields.
    #[serde(default)]
    pub max_len: Option<u32>,
}

/// Migration step for a payload from one schema version to another.
///
/// Not consumed yet — the migration list is always empty.
/// `op` can be `"rename"`, `"add"`, `"remove"`, or `"default"` when migration is implemented.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MigrationStep {
    /// Migration operation (e.g. `"rename"`, `"add"`, `"remove"`).
    pub op: String,

    /// Affected field.
    pub field: String,

    /// New value/key (depending on the operation).
    #[serde(default)]
    pub to: Option<String>,

    /// Default value (for the `"add"` operation).
    #[serde(default)]
    pub default: Option<toml::Value>,
}

/// Loads and returns the schema for `(override_type, schema_version)`.
///
/// The expected file is `schemas/overrides/<override_type>-v<version>.toml`.
/// Returns `None` if the file does not exist in the embedded directory.
///
/// # Example
///
/// ```rust
/// use gradatum_core::schema_registry::lookup;
///
/// let schema = lookup("metadata", 1).expect("metadata-v1 must be embedded");
/// assert_eq!(schema.override_type, "metadata");
/// ```
pub fn lookup(override_type: &str, version: u32) -> Option<OverrideSchema> {
    let path = format!("{override_type}-v{version}.toml");
    let file = SCHEMAS_DIR.get_file(&path)?;
    let content = file.contents_utf8()?;
    toml::from_str(content).ok()
}

/// Validates a TOML payload for `(override_type, version)` against its schema.
///
/// Stub — always returns `Ok`. Full validation (required fields, types,
/// `max_len` constraints) is planned for a future release.
///
/// # Note
///
/// `tracing` is not a dependency of `gradatum-core` — any observable warning must be
/// emitted by the consumer (e.g. `gradatum-server` handlers) after the call.
#[allow(unused_variables)]
pub fn validate_payload(
    override_type: &str,
    version: u32,
    payload_toml: &str,
) -> Result<(), ValidationError> {
    // Stub : schema validation not implemented — v0.5.0 F-38 schema enforcement.
    // Tout appel ici passe sans validation — intentionnel jusqu'à implémentation complète.
    Ok(())
}

/// Migrates a TOML payload from `from_version` to `to_version`.
///
/// Stub — returns the payload unchanged. Full migration (via `MigrationStep`) is planned for a future release.
#[allow(unused_variables)]
pub fn migrate_payload(
    override_type: &str,
    from: u32,
    to: u32,
    payload_toml: &str,
) -> Result<String, MigrationError> {
    Ok(payload_toml.to_string())
}

/// Validation error for an override payload against its schema.
#[derive(Debug, Error)]
pub enum ValidationError {
    /// Field not declared in the schema.
    #[error("champ inconnu : {0}")]
    UnknownField(String),

    /// Required field absent from the payload.
    #[error("champ obligatoire manquant : {0}")]
    MissingRequired(String),

    /// Field type incompatible with the schema.
    #[error("type incorrect pour le champ {0} : attendu {1}")]
    TypeMismatch(String, String),

    /// Maximum length exceeded.
    #[error("longueur max dépassée pour le champ {0} : obtenu {1}")]
    MaxLenExceeded(String, u32),
}

/// Migration error for an override payload.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// Schema not found in the embedded registry.
    #[error("schéma introuvable : {0}-v{1}")]
    SchemaNotFound(String, u32),

    /// Irreversible migration step.
    #[error("migration irréversible à l'étape {0}")]
    Irreversible(String),
}
