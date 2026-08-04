//! History hash for the Copy-on-Write (CoW) versioning scheme.
//!
//! `sha256_for_history` produces a stable hash over the *semantic* content of a note:
//! it excludes fields that change during autonomous processing (curator, jobs) to
//! avoid creating spurious CoW snapshots in `.history/` on every job pass.
//!
//! ## Excluded fields (`HISTORY_EXCLUDED_FIELDS`)
//!
//! Two categories:
//! 1. **`frontmatter.extra` (ExtraFields) keys**: `processed`, `covered-by`,
//!    `derived-into`, `rank` — written by curator/warden jobs without modifying the
//!    semantic content of the note.
//! 2. **Struct field `updated`** (`Option<DateTime<Utc>>`) — refreshed on every
//!    `write_note_inner`, even when the content is unchanged.
//!
//! ## Algorithm
//!
//! ```text
//! filtered_fm = frontmatter.clone() { extra without excluded keys, updated = None }
//! canon       = JCS(filtered_fm)          // serde_jcs — same canonicaliser as ContentHash
//! hash        = SHA-256(canon ++ b"\n---\n" ++ body_utf8)
//! ```
//!
//! Uses `serde_jcs` for consistency with [`crate::identity::ContentHash::compute`].
//!
//! ## Non-monotone
//!
//! This hash does NOT replace `NoteVersion` (monotonic counter).
//! The CoW snapshot is triggered by the *difference* in hash between `existing` and `new`,
//! not by a monotonic counter.

use sha2::{Digest, Sha256};

use crate::frontmatter::Frontmatter;
use crate::note::Note;

/// Extra keys and struct fields excluded from the history hash.
///
/// These fields are updated by processing jobs (curator, warden) without modifying
/// the semantic content of the note. Including them would trigger spurious
/// `.history/` snapshots on every job pass.
///
/// - `processed`, `covered-by`, `derived-into`, `rank`: keys in `ExtraFields`.
/// - `updated`: struct field `Frontmatter::updated` (`Option<DateTime<Utc>>`).
pub const HISTORY_EXCLUDED_FIELDS: &[&str] =
    &["processed", "covered-by", "derived-into", "updated", "rank"];

/// Returns a filtered copy of `frontmatter` for the history hash.
///
/// Two transformations:
/// 1. Removes `extra` keys listed in `HISTORY_EXCLUDED_FIELDS`.
/// 2. Sets `updated = None` (struct field, not an extra key).
///
/// `HISTORY_EXCLUDED_FIELDS` includes `"updated"` to document the intent,
/// but the explicit `None` assignment here is what handles the struct field.
fn frontmatter_for_history(fm: &Frontmatter) -> Frontmatter {
    let mut filtered = fm.clone();

    // Clear the modification timestamp — it changes on every write even when content is identical.
    filtered.updated = None;

    // Remove excluded extra keys (processing operations, not semantic content).
    if let Some(extra_map) = filtered.extra.0.as_mut() {
        for key in HISTORY_EXCLUDED_FIELDS {
            // "updated" is a struct field, not an extra key — no need to look for it here.
            if *key != "updated" {
                extra_map.remove(*key);
            }
        }
        // Clear the Box when the map is now empty (memory optimisation, consistent with ExtraFields::is_empty).
        if extra_map.is_empty() {
            filtered.extra.0 = None;
        }
    }

    filtered
}

/// Computes the SHA-256 hash of the semantic content of a note (excluding processing fields).
///
/// ## Algorithm
///
/// ```text
/// filtered_fm = frontmatter_for_history(&note.frontmatter)
/// canon       = JCS(filtered_fm)                           // serde_jcs RFC 8785
/// hash        = SHA-256(canon_bytes ++ b"\n---\n" ++ body_utf8)
/// ```
///
/// Uses the same separator and JCS canonicaliser as
/// [`crate::identity::ContentHash::compute`] to ensure consistency for tools
/// that cross-compare both hash types.
///
/// ## Panics
///
/// Never panics in production. `serde_jcs::to_string` returns an error only if
/// the serialised value contains a non-finite float (`f64::NAN` / `f64::INFINITY`).
/// `Frontmatter` contains no `f32`/`f64` struct fields; the `extra` map may hold
/// `toml::Value::Float` values, but TOML rejects NaN/Infinity at parse time
/// (RFC 3.3 — floats must be finite). A well-formed `Frontmatter` is therefore
/// always serialisable as JCS — same invariant as [`crate::identity::ContentHash::compute`].
pub fn sha256_for_history(note: &Note) -> [u8; 32] {
    let filtered = frontmatter_for_history(&note.frontmatter);

    // JCS RFC 8785: sorted keys + canonical IEEE 754 floats.
    // Same canonicaliser as ContentHash → reproducible across tools.
    let canon = serde_jcs::to_string(&filtered).expect(
        "Filtered frontmatter is always JCS-serializable: \
         no f64::NAN/INFINITY possible (TOML RFC 3.3 guarantees finite floats)",
    );

    let mut h = Sha256::new();
    h.update(canon.as_bytes());
    h.update(b"\n---\n");
    h.update(note.body.markdown.as_bytes());

    h.finalize().into()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::ExtraFields;
    use crate::identity::{ContentHash, NoteId, NoteVersion};
    use crate::note::{Note, NoteBody};
    use crate::scope::VaultId;
    use crate::section::Section;
    use crate::status::NoteStatus;
    use chrono::Utc;

    /// Construit une Note minimale avec le `body` fourni.
    fn note_for_test(body: &str) -> Note {
        let fm = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section: Section::Decisions,
            status: NoteStatus::Draft,
            status_reason: None,
            status_changed: None,
            tags: Default::default(),
            author: None,
            created: Utc::now(),
            updated: None,
            extra: ExtraFields::empty(),
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };
        let body_obj = NoteBody {
            markdown: body.to_string(),
        };
        let hash = ContentHash::compute(&fm, body);
        Note {
            id: NoteId::new(),
            frontmatter: fm,
            body: body_obj,
            version: NoteVersion::initial(),
            content_hash: hash,
            integrity_signature: None,
        }
    }

    /// Ajouter une clé extra exclue (`processed`) ne doit PAS changer le hash d'historique.
    #[test]
    fn history_hash_ignores_excluded_extra_key_processed() {
        let note_a = note_for_test("corps de la note");
        let mut note_b = note_a.clone();
        note_b
            .frontmatter
            .extra
            .insert("processed".to_string(), toml::Value::Boolean(true));

        assert_eq!(
            sha256_for_history(&note_a),
            sha256_for_history(&note_b),
            "L'ajout de la clé extra 'processed' ne doit pas changer le hash d'historique"
        );
    }

    /// Ajouter `covered-by` (clé extra exclue) ne doit pas changer le hash.
    #[test]
    fn history_hash_ignores_excluded_extra_key_covered_by() {
        let note_a = note_for_test("corps");
        let mut note_b = note_a.clone();
        note_b.frontmatter.extra.insert(
            "covered-by".to_string(),
            toml::Value::String("01ABCDEF".to_string()),
        );

        assert_eq!(
            sha256_for_history(&note_a),
            sha256_for_history(&note_b),
            "L'ajout de 'covered-by' ne doit pas changer le hash d'historique"
        );
    }

    /// Ajouter `rank` (clé extra exclue) ne doit pas changer le hash.
    #[test]
    fn history_hash_ignores_excluded_extra_key_rank() {
        let note_a = note_for_test("corps");
        let mut note_b = note_a.clone();
        note_b
            .frontmatter
            .extra
            .insert("rank".to_string(), toml::Value::Float(0.9));

        assert_eq!(
            sha256_for_history(&note_a),
            sha256_for_history(&note_b),
            "L'ajout de 'rank' ne doit pas changer le hash d'historique"
        );
    }

    /// Le champ `updated` (champ struct) ne doit pas influer sur le hash d'historique.
    #[test]
    fn history_hash_ignores_updated_field() {
        let mut note_a = note_for_test("corps");
        note_a.frontmatter.updated = None;
        let mut note_b = note_a.clone();
        note_b.frontmatter.updated = Some(Utc::now());

        assert_eq!(
            sha256_for_history(&note_a),
            sha256_for_history(&note_b),
            "Le champ 'updated' ne doit pas influer sur le hash d'historique"
        );
    }

    /// Modifier le body DOIT changer le hash d'historique.
    #[test]
    fn history_hash_changes_on_body_modification() {
        let note_a = note_for_test("corps original");
        let note_b = note_for_test("corps MODIFIÉ");

        assert_ne!(
            sha256_for_history(&note_a),
            sha256_for_history(&note_b),
            "Un changement de body doit changer le hash d'historique"
        );
    }

    /// Modifier le titre (section ici — le titre vit dans ContentHash, pas dans Frontmatter titre)
    /// → tester via section : changer la section doit changer le hash.
    #[test]
    fn history_hash_changes_on_section_change() {
        let note_a = note_for_test("corps");
        let mut note_b = note_a.clone();
        note_b.frontmatter.section = Section::Reasoning;

        assert_ne!(
            sha256_for_history(&note_a),
            sha256_for_history(&note_b),
            "Un changement de section doit changer le hash d'historique"
        );
    }

    /// Une clé extra non exclue doit bien changer le hash.
    #[test]
    fn history_hash_changes_on_non_excluded_extra_key() {
        let note_a = note_for_test("corps");
        let mut note_b = note_a.clone();
        note_b.frontmatter.extra.insert(
            "custom-field".to_string(),
            toml::Value::String("valeur".to_string()),
        );

        assert_ne!(
            sha256_for_history(&note_a),
            sha256_for_history(&note_b),
            "Une clé extra non exclue doit changer le hash d'historique"
        );
    }

    /// Stabilité : appels successifs sur la même note donnent le même hash.
    #[test]
    fn history_hash_is_deterministic() {
        let note = note_for_test("contenu stable");
        let h1 = sha256_for_history(&note);
        let h2 = sha256_for_history(&note);
        assert_eq!(h1, h2, "sha256_for_history doit être déterministe");
    }
}
