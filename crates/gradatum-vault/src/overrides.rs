//! Override de métadonnées de note — `NoteMetadataOverride`.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.8.
//!
//! ## Phase 1
//!
//! Seul override implémenté en Phase 1 : `NoteMetadataOverride` (type discriminant `"metadata"`).
//! Implémente `Overridable` et `OverridePayload` pour s'intégrer dans la table générique
//! `note_overrides` (décision Q7/B20).
//!
//! ## FrontmatterPatch::resolve
//!
//! Tous les champs du patch sont optionnels — seuls les champs présents (`Some(...)`)
//! sont appliqués sur le frontmatter de base. Permet des patches partiels.
//!
//! La résolution est **pure et sans effet de bord** — le frontmatter de base n'est
//! pas modifié, on retourne une nouvelle valeur clonée.
//!
//! ## Tags — SmallVec
//!
//! `Frontmatter.tags` est `SmallVec<[Tag; 4]>`. Les méthodes `contains`, `push` et
//! `retain` sont disponibles via l'implémentation `Deref<Target = [Tag]>` + `SmallVec` API.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::overrides::{FrontmatterPatch, Overridable, OverrideMeta, OverridePayload};

/// Override de métadonnées pour une note Gradatum.
///
/// Applique un `FrontmatterPatch` partiel sur le frontmatter d'une note dans un scope donné.
/// Stocké en TOML dans la table générique `note_overrides` sous le type discriminant `"metadata"`.
///
/// ## Champs
///
/// - `meta` : métadonnées communes (note_id, scope, priorité, auteur, timestamp).
/// - `patch` : delta à appliquer (section, tags, status, auteur, raison).
/// - `extra_overlay` : champs custom pour forward-compat (jamais appliqués par Phase 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteMetadataOverride {
    /// Métadonnées communes — stockées en colonnes SQL dédiées.
    pub meta: OverrideMeta,
    /// Delta de métadonnées à appliquer.
    pub patch: FrontmatterPatch,
    /// Champs custom pour extensibilité future (Phase 2+).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_overlay: BTreeMap<String, toml::Value>,
}

impl OverridePayload for NoteMetadataOverride {
    const OVERRIDE_TYPE: &'static str = "metadata";
    const SCHEMA_VERSION: gradatum_core::frontmatter::SchemaVersion = 1;
}

impl Overridable for NoteMetadataOverride {
    type Patch = FrontmatterPatch;
    type Output = Frontmatter;

    /// Applique le patch sur le frontmatter de base et retourne le frontmatter effectif.
    ///
    /// ## Règles de résolution
    ///
    /// - `section` : remplace si `Some`.
    /// - `tags_add` : ajoute chaque tag si absent (pas de doublon).
    /// - `tags_remove` : supprime chaque tag de la liste.
    /// - `status` : remplace si `Some`.
    /// - `author_override` : remplace si `Some`.
    /// - `status_reason` : remplace si `Some`.
    ///
    /// ## SmallVec
    ///
    /// `tags` est `SmallVec<[Tag; 4]>` — `contains`, `push` et `retain` sont disponibles
    /// via l'API standard `Vec`-like de SmallVec.
    fn resolve(base: &Frontmatter, patch: &FrontmatterPatch) -> Frontmatter {
        let mut effective = base.clone();

        if let Some(section) = patch.section {
            effective.section = section;
        }

        for tag in &patch.tags_add {
            if !effective.tags.contains(tag) {
                effective.tags.push(tag.clone());
            }
        }

        effective.tags.retain(|t| !patch.tags_remove.contains(t));

        if let Some(status) = patch.status {
            effective.status = status;
        }

        if let Some(ref author) = patch.author_override {
            effective.author = Some(author.clone());
        }

        if patch.status_reason.is_some() {
            effective.status_reason = patch.status_reason.clone();
        }

        effective
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;
    use gradatum_core::tag::Tag;

    fn build_frontmatter(status: NoteStatus) -> Frontmatter {
        Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section: Section::Decisions,
            status,
            status_reason: None,
            status_changed: None,
            tags: Default::default(),
            author: None,
            created: Utc::now(),
            updated: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn resolve_applies_status_change() {
        let base = build_frontmatter(NoteStatus::Draft);
        let patch = FrontmatterPatch {
            status: Some(NoteStatus::Live),
            ..Default::default()
        };
        let effective = NoteMetadataOverride::resolve(&base, &patch);
        assert_eq!(effective.status, NoteStatus::Live);
        // Autres champs inchangés
        assert_eq!(effective.section, Section::Decisions);
    }

    #[test]
    fn resolve_appends_tags_no_duplicate() {
        let mut base = build_frontmatter(NoteStatus::Draft);
        base.tags.push(Tag::new("rust").unwrap());

        let patch = FrontmatterPatch {
            tags_add: vec![Tag::new("rust").unwrap(), Tag::new("gradatum").unwrap()],
            ..Default::default()
        };
        let effective = NoteMetadataOverride::resolve(&base, &patch);
        // "rust" ne doit pas être dupliqué, "gradatum" doit être ajouté
        assert_eq!(effective.tags.len(), 2);
        assert!(effective.tags.contains(&Tag::new("rust").unwrap()));
        assert!(effective.tags.contains(&Tag::new("gradatum").unwrap()));
    }

    #[test]
    fn resolve_removes_tags() {
        let mut base = build_frontmatter(NoteStatus::Draft);
        base.tags.push(Tag::new("old-tag").unwrap());
        base.tags.push(Tag::new("keep-tag").unwrap());

        let patch = FrontmatterPatch {
            tags_remove: vec![Tag::new("old-tag").unwrap()],
            ..Default::default()
        };
        let effective = NoteMetadataOverride::resolve(&base, &patch);
        assert_eq!(effective.tags.len(), 1);
        assert!(effective.tags.contains(&Tag::new("keep-tag").unwrap()));
        assert!(!effective.tags.contains(&Tag::new("old-tag").unwrap()));
    }

    #[test]
    fn resolve_empty_patch_is_identity() {
        let base = build_frontmatter(NoteStatus::Live);
        let patch = FrontmatterPatch::default();
        let effective = NoteMetadataOverride::resolve(&base, &patch);
        assert_eq!(effective.status, base.status);
        assert_eq!(effective.section, base.section);
        assert_eq!(effective.tags, base.tags);
    }
}
