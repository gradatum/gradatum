//! Registre de schémas d'overrides — embedded via `include_dir!`.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.14.
//!
//! ## Design
//!
//! Les schémas TOML sont embarqués dans le binaire à la compilation (`include_dir!`).
//! Aucun accès filesystem au runtime → pas de dépendance sur le répertoire de travail.
//!
//! Structure : `schemas/overrides/<override_type>-v<schema_version>.toml`
//!
//! Phase 1 : 4 schémas embarqués :
//! - `metadata-v1.toml` — stable, owner `gradatum-vault`, Phase 1
//! - `acl-v1.toml`      — experimental, owner `gradatum-acl-policy`, Phase 2
//! - `index-v1.toml`    — experimental, owner `gradatum-index`, Phase 3
//! - `score-v1.toml`    — experimental, owner `gradatum-curator`, Phase 4
//!
//! ## Validation et migration (Phase 1 = stubs)
//!
//! `validate_payload` et `migrate_payload` retournent toujours `Ok` en Phase 1.
//! Ils seront implémentés en Phase 2+ avec la logique complète basée sur `FieldSpec`.
//!
//! ## Décision
//!
//! Q7/B20 brainstorming the maintainer 2026-05-03 — schéma registry TOML embedded,
//! pas de registry externe, pas de réseau au runtime.

use std::collections::BTreeMap;

use include_dir::{include_dir, Dir};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Répertoire des schémas embarqués — relatif au CARGO_MANIFEST_DIR.
static SCHEMAS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/schemas/overrides");

/// Schéma d'un override — désérialisé depuis les fichiers TOML embarqués.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OverrideSchema {
    /// Discriminant du type d'override (ex. `"metadata"`).
    pub override_type: String,

    /// Version du schéma.
    pub schema_version: u32,

    /// Crate propriétaire du schéma.
    pub owner_crate: String,

    /// Phase Gradatum dans laquelle ce schéma est utilisé.
    pub phase: u8,

    /// Statut de stabilité du schéma.
    pub status: SchemaStatus,

    /// Spécifications des champs du payload.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldSpec>,

    /// Directives de migration depuis une version précédente.
    #[serde(default)]
    pub migration: BTreeMap<String, Vec<MigrationStep>>,
}

/// Statut de stabilité d'un schéma d'override.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SchemaStatus {
    /// Schéma stable — API guarantie dans la phase courante.
    Stable,
    /// Schéma en cours de stabilisation.
    Unstable,
    /// Schéma expérimental — peut changer sans notice.
    Experimental,
    /// Schéma déprécié — sera supprimé dans la prochaine version majeure.
    Deprecated,
}

/// Spécification d'un champ du payload override.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FieldSpec {
    /// Type du champ (ex. `"enum:Section"`, `"array:Tag"`, `"string"`, `"AuthorRef"`).
    #[serde(rename = "type")]
    pub field_type: String,

    /// Le champ est-il obligatoire dans le payload ?
    #[serde(default)]
    pub required: bool,

    /// Valeur par défaut si absent.
    #[serde(default)]
    pub default: Option<toml::Value>,

    /// Longueur maximale pour les champs de type `"string"`.
    #[serde(default)]
    pub max_len: Option<u32>,
}

/// Étape de migration d'un payload d'une version à une autre.
///
/// Phase 1 = non utilisé (migration est toujours une liste vide).
/// Phase 2+ : `op` peut être `"rename"`, `"add"`, `"remove"`, `"default"`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MigrationStep {
    /// Opération de migration (ex. `"rename"`, `"add"`, `"remove"`).
    pub op: String,

    /// Champ concerné.
    pub field: String,

    /// Nouvelle valeur/clé (selon l'opération).
    #[serde(default)]
    pub to: Option<String>,

    /// Valeur par défaut (pour l'opération `"add"`).
    #[serde(default)]
    pub default: Option<toml::Value>,
}

/// Charge et retourne le schéma pour `(override_type, schema_version)`.
///
/// Le fichier attendu est `schemas/overrides/<override_type>-v<version>.toml`.
/// Retourne `None` si le fichier n'existe pas dans le répertoire embarqué.
///
/// # Exemple
///
/// ```rust
/// use gradatum_core::schema_registry::lookup;
///
/// let schema = lookup("metadata", 1).expect("metadata-v1 doit être embarqué");
/// assert_eq!(schema.override_type, "metadata");
/// ```
pub fn lookup(override_type: &str, version: u32) -> Option<OverrideSchema> {
    let path = format!("{override_type}-v{version}.toml");
    let file = SCHEMAS_DIR.get_file(&path)?;
    let content = file.contents_utf8()?;
    toml::from_str(content).ok()
}

/// Valide le payload TOML pour `(override_type, version)` contre son schéma.
///
/// **Phase 1 = stub** — retourne toujours `Ok`. La validation complète est Phase 2+.
///
/// Phase 2+ : vérifiera les champs obligatoires, les types, les contraintes max_len.
#[allow(unused_variables)]
pub fn validate_payload(
    override_type: &str,
    version: u32,
    payload_toml: &str,
) -> Result<(), ValidationError> {
    Ok(())
}

/// Migre un payload TOML de `from_version` vers `to_version`.
///
/// **Phase 1 = stub** — retourne le payload inchangé. La migration complète est Phase 2+.
///
/// Phase 2+ : appliquera les `MigrationStep` du schéma cible.
#[allow(unused_variables)]
pub fn migrate_payload(
    override_type: &str,
    from: u32,
    to: u32,
    payload_toml: &str,
) -> Result<String, MigrationError> {
    Ok(payload_toml.to_string())
}

/// Erreur de validation d'un payload override contre son schéma.
#[derive(Debug, Error)]
pub enum ValidationError {
    /// Champ non déclaré dans le schéma.
    #[error("champ inconnu : {0}")]
    UnknownField(String),

    /// Champ obligatoire absent.
    #[error("champ obligatoire manquant : {0}")]
    MissingRequired(String),

    /// Type du champ incompatible avec le schéma.
    #[error("type incorrect pour le champ {0} : attendu {1}")]
    TypeMismatch(String, String),

    /// Longueur maximale dépassée.
    #[error("longueur max dépassée pour le champ {0} : obtenu {1}")]
    MaxLenExceeded(String, u32),
}

/// Erreur de migration d'un payload override.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// Schéma introuvable dans le registre embarqué.
    #[error("schéma introuvable : {0}-v{1}")]
    SchemaNotFound(String, u32),

    /// Étape de migration irréversible.
    #[error("migration irréversible à l'étape {0}")]
    Irreversible(String),
}
