//! Helpers partagés entre les tests d'intégration de gradatum-index.

use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

/// Construit une `Note` minimale valide pour les tests.
///
/// `vault_id` est le tenant. `body` est le texte Markdown brut.
/// Le `ContentHash` est calculé via `ContentHash::compute`.
#[allow(dead_code)]
pub fn make_note(vault_id: &str, section: Section, status: NoteStatus, body: &str) -> Note {
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
        locus: None,
        section,
        status,
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
    let note_body = NoteBody {
        markdown: body.to_string(),
    };
    let content_hash = ContentHash::compute(&frontmatter, body);
    Note {
        id: NoteId::new(),
        frontmatter,
        body: note_body,
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    }
}

/// Construit une `FileChecksumEntry` depuis un fichier sur disque.
#[allow(dead_code)]
pub fn make_checksum_entry(
    path: &std::path::Path,
    relative: &str,
    file_kind: gradatum_core::index::FileKind,
) -> gradatum_core::index::FileChecksumEntry {
    use sha2::Digest as _;

    let bytes = std::fs::read(path).unwrap();
    let metadata = std::fs::metadata(path).unwrap();
    let size = metadata.len();

    let prefix_len = bytes.len().min(4096);
    let prefix_hash: [u8; 32] = sha2::Sha256::digest(&bytes[..prefix_len]).into();
    let full_hash: [u8; 32] = sha2::Sha256::digest(&bytes).into();

    gradatum_core::index::FileChecksumEntry {
        relative_path: relative.to_string(),
        file_kind,
        expected_size: size,
        expected_hash_prefix_4kb: prefix_hash,
        expected_hash: full_hash,
        expected_mtime: 0, // fixé à 0 dans les tests (non discriminant pour Phase 1)
        last_verified: 0,
    }
}
