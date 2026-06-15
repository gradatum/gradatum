//! Canonical Gradatum note.
//!
//! ## Design
//!
//! `Note` is the central pivot of the Gradatum data model.
//! - `NoteId`: ULID primary key.
//! - `Frontmatter`: canonical metadata (section, status, tags, author, etc.).
//! - `NoteBody`: raw Markdown content.
//! - `NoteVersion`: monotonic counter (incremented by the worker on every write).
//! - `ContentHash`: JCS SHA-256 of (frontmatter + body) — drift detection.
//! - `IntegritySignature`: always `None` without an auth layer. HMAC/Ed25519 available via `gradatum-acl-auth`.
//!
//! ## EffectiveNote
//!
//! `EffectiveNote` = `Note` + overrides resolved in the bearer scope.
//! Same structure as `Note` with a patched `Frontmatter`.
//! `gradatum-vault::get_effective_note()` produces the `EffectiveNote` by applying
//! active `FrontmatterPatch` entries in priority order.

use serde::{Deserialize, Serialize};

use crate::frontmatter::Frontmatter;
use crate::identity::{ContentHash, IntegritySignature, NoteId, NoteVersion};

/// Note body — raw Markdown content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteBody {
    /// Markdown content of the note.
    pub markdown: String,
}

/// Canonical note as stored in the vault.
///
/// Source of truth: the `.md` file on disk.
/// The SQLite index is derived from the file and is always rebuildable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// FIXME(v1.0.0-silver): renommer `Note` → `Document` + aligner `DocumentStore::write_note` → `write`.
// Bloqué par : blast radius workspace (28 crates, 900+ tests). Tracked : Étape 0.2+.
pub struct Note {
    /// ULID primary key.
    pub id: NoteId,

    /// Canonical metadata.
    pub frontmatter: Frontmatter,

    /// Markdown body.
    pub body: NoteBody,

    /// Monotonic version — incremented on every write.
    pub version: NoteVersion,

    /// JCS SHA-256 hash of (frontmatter + body) — drift detection.
    pub content_hash: ContentHash,

    /// Cryptographic signature — `None` without an auth layer.
    ///
    /// HMAC-SHA256 or Ed25519 available via `gradatum-acl-auth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity_signature: Option<IntegritySignature>,
}

impl Note {
    /// Verify drift integrity by recomputing the [`ContentHash`] and comparing it
    /// against the stored value.
    ///
    /// Used by `gradatum-vault` drift scanner and any consumer that
    /// wants to assert a `Note` is internally consistent before reading its body
    /// or trusting its frontmatter.
    ///
    /// Detects accidental drift (accidental edit). Tamper-proof
    /// verification via [`IntegritySignature`] (HMAC-SHA256 / Ed25519)
    /// is available in `gradatum-acl-auth`.
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

/// Effective note = `Note` + overrides resolved in the bearer scope.
///
/// Produced by `gradatum-vault::get_effective_note()`.
/// Same structure as `Note` with `Frontmatter` patched by active `FrontmatterPatch` entries.
///
/// Intentionally not serialisable — this is an ephemeral in-memory view,
/// never persisted to the vault.
#[derive(Debug, Clone)]
pub struct EffectiveNote {
    /// Primary key — identical to the source `Note`.
    pub id: NoteId,

    /// Frontmatter with overrides applied in priority order.
    pub frontmatter: Frontmatter,

    /// Markdown body — identical to the source `Note`.
    pub body: NoteBody,

    /// Version — identical to the source `Note`.
    pub version: NoteVersion,

    /// Content hash — identical to the source `Note`.
    pub content_hash: ContentHash,
}
