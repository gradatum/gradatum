//! Note canonique Gradatum.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.3.
//!
//! ## Design
//!
//! `Note` = pivot central du modèle de données Gradatum.
//! - `NoteId` : clé primaire ULID.
//! - `Frontmatter` : métadonnées canoniques (section, status, tags, author, etc.).
//! - `NoteBody` : contenu Markdown brut.
//! - `NoteVersion` : compteur monotone (incrémenté par le worker à chaque écriture).
//! - `ContentHash` : SHA-256 JCS du (frontmatter + body) — drift detection Phase 1.
//! - `IntegritySignature` : Phase 1 = toujours `None`. Phase 2+ HMAC/Ed25519.
//!
//! ## EffectiveNote
//!
//! `EffectiveNote` = `Note` + overrides résolus dans le scope du bearer.
//! Phase 1 : identique à `Note` avec `Frontmatter` patchée.
//! `gradatum-vault::get_effective_note()` produit l'`EffectiveNote` en appliquant
//! les `FrontmatterPatch` actifs dans l'ordre de priorité.

use serde::{Deserialize, Serialize};

use crate::frontmatter::Frontmatter;
use crate::identity::{ContentHash, IntegritySignature, NoteId, NoteVersion};

/// Corps de la note — contenu Markdown brut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteBody {
    /// Contenu Markdown de la note.
    pub markdown: String,
}

/// Note canonique telle que stockée dans le vault.
///
/// Source de vérité : le fichier `.md` sur disque (OpenDAL-friendly §2.1).
/// L'index SQLite est dérivé du fichier — il est toujours reconstruisable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// FIXME(v1.0.0-silver): renommer `Note` → `Document` + aligner `DocumentStore::write_note` → `write`.
// Bloqué par : blast radius workspace (28 crates, 900+ tests). Tracked : Étape 0.2+.
pub struct Note {
    /// Clé primaire ULID.
    pub id: NoteId,

    /// Métadonnées canoniques.
    pub frontmatter: Frontmatter,

    /// Corps Markdown.
    pub body: NoteBody,

    /// Version monotone — incrémentée à chaque écriture.
    pub version: NoteVersion,

    /// Hash SHA-256 JCS du (frontmatter + body) — drift detection.
    pub content_hash: ContentHash,

    /// Signature cryptographique — Phase 1 toujours `None`.
    ///
    /// Phase 2+ : HMAC-SHA256 ou Ed25519 dans `gradatum-acl-auth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity_signature: Option<IntegritySignature>,
}

impl Note {
    /// Verify drift integrity by recomputing the [`ContentHash`] and comparing it
    /// against the stored value.
    ///
    /// Used by `gradatum-vault` drift scanner (spec §5.3) and any consumer that
    /// wants to assert a `Note` is internally consistent before reading its body
    /// or trusting its frontmatter.
    ///
    /// Phase 1 = drift detection (accidental edit). Phase 2+ adds tamper-proof
    /// verification via [`IntegritySignature`] (HMAC-SHA256 / Ed25519) — that
    /// check lives in `gradatum-acl-auth`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::DriftError::ContentHashMismatch`] if the
    /// recomputed hash does not equal the stored `content_hash`.
    pub fn verify_integrity(&self) -> Result<(), crate::error::DriftError> {
        let computed = ContentHash::compute(&self.frontmatter, &self.body.markdown);
        if computed == self.content_hash {
            Ok(())
        } else {
            Err(crate::error::DriftError::ContentHashMismatch {
                stored: self.content_hash,
                computed,
            })
        }
    }
}

/// Note effective = Note + overrides résolus dans le scope bearer.
///
/// Produite par `gradatum-vault::get_effective_note()`.
/// Phase 1 : même structure que `Note` avec `Frontmatter` patchée par les
/// `FrontmatterPatch` actifs.
///
/// Non-sérialisable intentionnellement — c'est une vue éphémère en mémoire,
/// jamais persistée dans le vault.
#[derive(Debug, Clone)]
pub struct EffectiveNote {
    /// Clé primaire — identique à la `Note` source.
    pub id: NoteId,

    /// Frontmatter avec overrides appliqués dans l'ordre de priorité.
    pub frontmatter: Frontmatter,

    /// Corps Markdown — identique à la `Note` source (Phase 1).
    pub body: NoteBody,

    /// Version — identique à la `Note` source.
    pub version: NoteVersion,

    /// Hash — identique à la `Note` source.
    pub content_hash: ContentHash,
}
