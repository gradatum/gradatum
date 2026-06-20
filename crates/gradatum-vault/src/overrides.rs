//! Note metadata override — `NoteMetadataOverride`.
//!
//! Only implemented override: `NoteMetadataOverride` (discriminant type `"metadata"`).
//! Implements `Overridable` and `OverridePayload` to integrate with the generic
//! `note_overrides` table.
//!
//! ## `FrontmatterPatch::resolve`
//!
//! All patch fields are optional — only fields present (`Some(...)`)
//! are applied to the base frontmatter, enabling partial patches.
//!
//! Resolution is **pure and side-effect-free** — the base frontmatter is not
//! modified; a new cloned value is returned.
//!
//! ## Tags — SmallVec
//!
//! `Frontmatter.tags` is `SmallVec<[Tag; 4]>`. The `contains`, `push`, and
//! `retain` methods are available via the `Deref<Target = [Tag]>` + `SmallVec` API.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::overrides::{FrontmatterPatch, Overridable, OverrideMeta, OverridePayload};

/// Metadata override for a Gradatum note.
///
/// Applies a partial `FrontmatterPatch` to a note's frontmatter within a given scope.
/// Stored as TOML in the generic `note_overrides` table under the discriminant type `"metadata"`.
///
/// ## Fields
///
/// - `meta`: common metadata (note_id, scope, priority, author, timestamp).
/// - `patch`: delta to apply (section, tags, status, author, reason).
/// - `extra_overlay`: custom fields for future extensibility (not applied).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteMetadataOverride {
    /// Common metadata — stored in dedicated SQL columns.
    pub meta: OverrideMeta,
    /// Metadata delta to apply.
    pub patch: FrontmatterPatch,
    /// Custom fields for future extensibility (not applied).
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

    /// Applies the patch to the base frontmatter and returns the effective frontmatter.
    ///
    /// ## Resolution rules
    ///
    /// - `section`: replaced if `Some`.
    /// - `tags_add`: each tag is appended if not already present (no duplicates).
    /// - `tags_remove`: each tag is removed from the list.
    /// - `status`: replaced if `Some`.
    /// - `author_override`: replaced if `Some`.
    /// - `status_reason`: replaced if `Some`.
    ///
    /// ## SmallVec
    ///
    /// `tags` is `SmallVec<[Tag; 4]>` — `contains`, `push`, and `retain` are available
    /// via the standard `Vec`-like SmallVec API.
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
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
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
