//! Helpers partagés entre les tests d'intégration de gradatum-index.

use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Construit une `Note` minimale valide pour les tests.
///
/// `vault_id` est le tenant. `body` est le texte Markdown brut.
/// Le `ContentHash` est calculé via `ContentHash::compute`.
#[allow(dead_code)]
pub fn make_note(vault_id: &str, section: Section, status: NoteStatus, body: &str) -> Note {
    // Délègue à la variante à ULID imposé avec un `NoteId` frais — DRY : un seul
    // constructeur de `Frontmatter`/`Note` (cf. [`make_note_with_id`]).
    make_note_with_id(vault_id, NoteId::new(), section, status, body)
}

// ---------------------------------------------------------------------------
// Harnais 2-vaults « flag ON » (C4-1e) — fixture réutilisable pour les tests
// d'isolation cross-vault. Deux vaults distincts contiennent des notes de MÊME
// ULID mais au contenu propre ; le régime multi-vault est purement local au test
// (aucune dépendance à la configuration serveur LIVE).
// ---------------------------------------------------------------------------

/// Vault principal conventionnel du harnais 2-vaults.
#[allow(dead_code)]
pub const VAULT_MAIN: &str = "main";

/// Second vault conventionnel du harnais 2-vaults (nom générique).
#[allow(dead_code)]
pub const VAULT_B: &str = "vault-b";

/// Construit un `SqliteIndex` en mémoire avec toutes les migrations appliquées
/// (jusqu'à `0032`, clé primaire composite `(vault_id, id)`).
///
/// Base de départ des tests d'isolation multi-vault : deux vaults distincts y sont
/// peuplés côté fixture via [`seed_colliding_note`], sans toucher la config LIVE.
///
/// # Panics
///
/// Panique si l'ouverture in-memory ou l'exécution des migrations échoue — invariant
/// de test : une base neuve migre toujours vers le dernier schéma.
#[allow(dead_code)]
pub async fn two_vault_index() -> SqliteIndex {
    SqliteIndex::open_in_memory()
        .await
        .expect("ouverture SqliteIndex in-memory + migrations (invariant test)")
}

/// Dérive un `NoteId` déterministe depuis une clé de test arbitraire.
///
/// La même clé produit le même ULID dans deux vaults distincts : c'est ce qui crée la
/// collision cross-vault volontaire (même `id`, `vault_id` différent).
#[allow(dead_code)]
pub fn colliding_note_id(id: &str) -> NoteId {
    NoteId::derived_from(id.as_bytes())
}

/// Insère dans `vault` une note dont l'ULID est dérivé déterministiquement de `id`
/// (cf. [`colliding_note_id`]) et dont le `title` est porté par la première ligne H1
/// du corps.
///
/// Le corps est écrit via `upsert_note`, seul chemin d'écriture **scopé par
/// construction** sur `(vault_id, id)` (`INSERT ... ON CONFLICT(vault_id, id)`). Le
/// harnais n'emprunte volontairement aucune méthode id-only encore sous revue C4-1e
/// (ex. `upsert_note_title`), afin que la fixture reste correcte indépendamment des
/// correctifs à venir. Le signal distinctif inter-vault est donc porté par le corps.
///
/// # Panics
///
/// Panique si l'upsert échoue — invariant de test.
#[allow(dead_code)]
pub async fn seed_colliding_note(idx: &SqliteIndex, vault: &str, id: &str, title: &str) {
    let note_id = colliding_note_id(id);
    // Le titre est la première ligne H1 du corps (comme une vraie note curée), ce qui
    // le rend lisible via `NoteRecord.body_text` sans dépendre de la colonne `title`.
    let body = format!("# {title}\n\ncorps de test — vault {vault}");
    let note = make_note_with_id(vault, note_id, Section::Reference, NoteStatus::Live, &body);
    idx.upsert_note(&note)
        .await
        .expect("upsert note de test scopée (vault_id, id)");
}

/// Extrait le titre H1 (`# …`) porté par la première ligne d'un corps de note.
///
/// Miroir de l'extraction de titre côté curation : renvoie `None` si la première
/// ligne n'est pas un H1. Utilisé par les tests pour asserter le titre d'une note
/// semée par [`seed_colliding_note`].
#[allow(dead_code)]
pub fn h1_title(body_text: &str) -> Option<&str> {
    body_text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("# "))
        .map(str::trim)
}

/// Variante de [`make_note`] à ULID imposé — nécessaire pour semer des notes à `id`
/// colliding dans plusieurs vaults.
#[allow(dead_code)]
pub fn make_note_with_id(
    vault_id: &str,
    id: NoteId,
    section: Section,
    status: NoteStatus,
    body: &str,
) -> Note {
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
    let content_hash = ContentHash::compute(&frontmatter, body);
    Note {
        id,
        frontmatter,
        body: NoteBody {
            markdown: body.to_string(),
        },
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
