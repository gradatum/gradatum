//! Frontmatter canonique d'une note Gradatum.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.3.
//!
//! ## Design
//!
//! - `Frontmatter` : struct principale sérialisée en YAML dans l'en-tête `.md`.
//! - `ExtraFields` : champs inconnus préservés verbatim (forward-compat B8).
//!   Allocation lazy — `None` si aucun champ extra (perf risk #3 spec).
//! - `tags: SmallVec<[Tag; 4]>` — inline jusqu'à 4 tags sans allocation heap (perf risk #2).
//!
//! ## Multi-tenancy
//!
//! `vault_id` est **mandatory** (invariant D10, C4). Pas de note sans tenant.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::collections::BTreeMap;

use crate::author::AuthorRef;
use crate::scope::{LocusId, VaultId};
use crate::section::Section;
use crate::status::NoteStatus;
use crate::tag::Tag;

/// Version du schéma frontmatter.
///
/// Incrémentée à chaque breaking change du format. Permet migration forward-compat
/// via `SchemaVersion::CURRENT` + `match schema_version { 1 => ..., 2 => ... }`.
pub type SchemaVersion = u32;

/// Champs inconnus préservés verbatim — allocation lazy.
///
/// Permet aux frontmatters du prédécesseur v1.x de round-tripper sans perte,
/// même si des champs custom non-canoniques sont présents (B8 forward-compat).
///
/// ## Perf
///
/// `Option<Box<BTreeMap<...>>>` évite toute allocation heap pour les notes sans extra.
/// La `Box` réduit la taille de `Frontmatter` dans le cas None (perf risk #3 spec §6.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtraFields(pub Option<Box<BTreeMap<String, toml::Value>>>);

impl ExtraFields {
    /// Retourne `true` si aucun champ extra n'est présent.
    pub fn is_empty(&self) -> bool {
        self.0.as_ref().is_none_or(|m| m.is_empty())
    }

    /// Construit un `ExtraFields` vide sans allocation.
    pub fn empty() -> Self {
        Self(None)
    }

    /// Insère un champ extra (alloue la map si nécessaire).
    ///
    /// # Contrainte JCS (P1-2 audit L0 2026-05-04)
    ///
    /// **`toml::Value::Datetime` est INTERDIT** dans `ExtraFields` si la note
    /// est destinée à être hashée via [`crate::identity::ContentHash::compute`].
    /// Le variant `Datetime` produit une sérialisation JSON non-portable en
    /// `toml 0.8.x` (représentation interne `{"$__toml_private_datetime": ...}`),
    /// ce qui casse la garantie spec §2.2 « hash bit-identique cross-language ».
    ///
    /// Pour stocker une date/heure dans `ExtraFields`, utiliser
    /// `toml::Value::String("2026-05-04T10:00:00Z".to_string())` (chaîne ISO 8601
    /// brute) au lieu de `toml::Value::Datetime(...)`.
    ///
    /// Phase 2+ : remplacement de `toml::Value` par `serde_json::Value` planifié
    /// (élimine la contrainte par construction — voir audit L0 recommandations).
    pub fn insert(&mut self, k: String, v: toml::Value) {
        self.0.get_or_insert_with(Default::default).insert(k, v);
    }

    /// Récupère la valeur d'un champ extra.
    pub fn get(&self, k: &str) -> Option<&toml::Value> {
        self.0.as_ref().and_then(|m| m.get(k))
    }
}

/// Frontmatter canonique d'une note Gradatum.
///
/// Sérialisée en YAML dans l'en-tête `---\n...\n---\n` du fichier Markdown.
/// Source de vérité pour le hash `ContentHash` (invariant #1).
///
/// ## Champs optionnels
///
/// Les champs `skip_serializing_if` sont omis en sérialisation si absents,
/// ce qui préserve la lisibilité des frontmatters minimalistes.
///
/// ## Compatibilité
///
/// Les champs YAML inconnus (ex. du prédécesseur v1.x) sont catchés par
/// `#[serde(flatten)]` dans la fixture YAML. Gradatum-markdown gère ce
/// catch au niveau du parser — Frontmatter lui-même expose `extra: ExtraFields`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frontmatter {
    /// Version du schéma. `1` pour Phase 1. Incrémenté sur breaking change.
    pub schema_version: SchemaVersion,

    /// Tenant mandatory (invariant D10). Alias UI : `vault`.
    pub vault_id: VaultId,

    /// Périmètre ACL sub-vault optionnel. `None` = scope vault root.
    /// `LocusId` finalisé en T03c (scope.rs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locus: Option<LocusId>,

    /// Section canonique de la note.
    pub section: Section,

    /// Statut du cycle de vie.
    pub status: NoteStatus,

    /// Raison optionnelle du statut courant (pour audit trail).
    ///
    /// Ex. : `"rejected by curator: low novelty"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,

    /// Timestamp du dernier changement de statut.
    ///
    /// Utilisé par les requêtes cron de decay/cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_changed: Option<DateTime<Utc>>,

    /// Tags de la note — inline SmallVec jusqu'à 4 tags sans allocation heap.
    ///
    /// Perf risk #2 spec §6.1 — typique < 4 tags, inline eliminates heap for 95% of notes.
    #[serde(default, skip_serializing_if = "SmallVec::is_empty")]
    pub tags: SmallVec<[Tag; 4]>,

    /// Auteur de la note.
    ///
    /// Optionnel en v0.1 pour compat avec les notes du prédécesseur v1.x sans auteur.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<AuthorRef>,

    /// Timestamp de création (immutable après premier commit).
    pub created: DateTime<Utc>,

    /// Timestamp de dernière modification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,

    /// Champs TOML inconnus préservés verbatim (forward-compat B8).
    ///
    /// Allocation lazy : aucune heap allocation si aucun extra (perf risk #3).
    /// Omis en sérialisation si vide.
    #[serde(default, skip_serializing_if = "ExtraFields::is_empty")]
    pub extra: ExtraFields,
}
