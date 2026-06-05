//! Override distribués — traits + types + FrontmatterPatch.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.8.
//!
//! ## Design
//!
//! Un override est une modification de métadonnée d'une note, scopée à un contexte
//! particulier (`OverrideScope`) et stockée dans la table générique `note_overrides`.
//!
//! ### Traits
//!
//! - `Overridable` : trait à Associated Types (pattern Iterator/Future) — décision Q1/B2.
//!   `type Patch` = delta à appliquer ; `type Output` = type résultant de la résolution.
//! - `OverridePayload` : contrat de stockage pour tout payload override (TOML embed + discriminant).
//!
//! ### FrontmatterPatch
//!
//! Le seul payload Phase 1 — `NoteMetadataOverride` vit dans `gradatum-vault` et implémente
//! `Overridable<Patch = FrontmatterPatch, Output = Frontmatter>`.
//! `FrontmatterPatch` est ici parce que `gradatum-vault` ET `gradatum-curator` le construisent.
//!
//! ## Décisions
//!
//! - Q1/B2 : Associated Types (composition-friendly, idiomatic Rust)
//! - Q7/B20 : 1 override actif par `(note, scope, type)` + table générique + trait `OverridePayload`
//! - §2.14 : schéma registry TOML embedded

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::author::AuthorRef;
use crate::frontmatter::SchemaVersion;
use crate::identity::NoteId;
use crate::scope::OverrideScope;

/// Métadonnées communes à tout override — stockées en colonnes dédiées dans `note_overrides`.
///
/// Le payload (struct concrète) est sérialisé en TOML dans la colonne `payload_toml`
/// et identifié par (`override_type`, `schema_version`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideMeta {
    /// Note à laquelle s'applique l'override.
    pub note_id: NoteId,

    /// Scope de l'override — détermine la priorité de résolution.
    pub scope: OverrideScope,

    /// Priorité de résolution : override de priorité plus haute gagne en cas de conflit.
    ///
    /// Phase 1 : `0` = priorité par défaut. Phase 2+ : utilisé par `OverrideResolver`.
    pub priority: u8,

    /// Auteur de l'override (humain, agent, système).
    pub created_by: AuthorRef,

    /// Timestamp de création de l'override.
    pub created_at: DateTime<Utc>,

    /// Raison optionnelle (audit trail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Trait de résolution d'override — composition par Associated Types.
///
/// Décision Q1/B2 brainstorming the maintainer 2026-05-03.
///
/// Pattern identique à `Iterator` (`type Item`) et `Future` (`type Output`) :
/// les crates aval peuvent contraindre `T: Overridable<Patch = FrontmatterPatch>` sans
/// mentionner le type concret d'override.
///
/// ## Implémentation attendue (Phase 1)
///
/// `gradatum-vault::NoteMetadataOverride` :
/// ```text
/// impl Overridable for NoteMetadataOverride {
///     type Patch  = FrontmatterPatch;
///     type Output = Frontmatter;
///     fn resolve(base: &Frontmatter, patch: &FrontmatterPatch) -> Frontmatter { … }
/// }
/// ```
pub trait Overridable {
    /// Type représentant le delta à appliquer sur la valeur de base.
    type Patch;
    /// Type de la valeur résultante après application du patch.
    type Output;

    /// Applique `patch` sur `base` et retourne la valeur résolue.
    ///
    /// Pur et sans effet de bord — `base` n'est pas modifié.
    fn resolve(base: &Self::Output, patch: &Self::Patch) -> Self::Output;
}

/// Contrat de stockage pour un payload override dans la table `note_overrides`.
///
/// Décision Q7/B20 — toute struct de payload override implémente ce trait pour
/// s'enregistrer dans le schema registry et se round-tripper via TOML.
///
/// ## Implémentation
///
/// ```rust,ignore
/// impl OverridePayload for NoteMetadataOverride {
///     const OVERRIDE_TYPE: &'static str = "metadata";
///     const SCHEMA_VERSION: SchemaVersion = 1;
/// }
/// ```
pub trait OverridePayload: Sized + Serialize + for<'de> Deserialize<'de> {
    /// Discriminant stable utilisé dans la colonne SQL `override_type`.
    ///
    /// Doit correspondre au fichier `schemas/overrides/<OVERRIDE_TYPE>-v<SCHEMA_VERSION>.toml`.
    const OVERRIDE_TYPE: &'static str;

    /// Version du schéma courante — doit correspondre au registry TOML embedded.
    const SCHEMA_VERSION: SchemaVersion;

    /// Sérialise le payload en TOML pretty pour stockage en BDD.
    ///
    /// # Erreurs
    ///
    /// Retourne une erreur si le type contient des valeurs non-sérialisables en TOML
    /// (ex. une enum avec données non supportées par toml_edit).
    fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Désérialise le payload depuis TOML (lecture BDD).
    ///
    /// # Erreurs
    ///
    /// Retourne une erreur si le TOML est malformé ou incompatible avec le type.
    fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

/// Patch de métadonnées pour `NoteMetadataOverride` (seul override Phase 1).
///
/// Tous les champs sont optionnels — seuls les champs présents sont appliqués lors
/// de la résolution. Permet des patches partiels sans re-spécifier la totalité du frontmatter.
///
/// Vit dans `gradatum-core` (pas dans `gradatum-vault`) parce que :
/// - `gradatum-vault` le construit lors de l'écriture d'overrides.
/// - `gradatum-curator` le construit lors de ses décisions de catégorisation.
/// - Évite une dépendance `curator → vault` (anti-cycle).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FrontmatterPatch {
    /// Nouvelle section à appliquer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<crate::section::Section>,

    /// Tags à ajouter à la liste courante.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_add: Vec<crate::tag::Tag>,

    /// Tags à retirer de la liste courante.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_remove: Vec<crate::tag::Tag>,

    /// Nouveau statut à appliquer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<crate::status::NoteStatus>,

    /// Override de l'auteur (ex. pour corriger une attribution erronée).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_override: Option<AuthorRef>,

    /// Raison du changement de statut (pour audit trail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}
