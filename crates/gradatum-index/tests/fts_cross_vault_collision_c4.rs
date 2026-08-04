//! C4-1d (P1 security review) — gap FTS : une collision d'ULID cross-vault NE DOIT PAS
//! clobber l'entrée `notes_fts` de `main`.
//!
//! ## Menace (le gap que toute la série C4 n'assertait pas)
//!
//! `notes.id` est `PRIMARY KEY` seul + `notes_fts` est external-content (`content=notes`,
//! content_rowid = rowid implicite de `notes`). La sync FTS (`sqlite.rs`) obtient le rowid
//! via `SELECT rowid FROM notes WHERE id = ?1` (id-only). En collision cross-vault, ce SELECT
//! renvoie le rowid de la ligne de `main` → l'`INSERT OR REPLACE notes_fts` écrase l'entrée FTS
//! de `main` avec le corps de `research` (InfoDisclosure : la recherche scopée `main` retourne le
//! contenu research + Tampering : le vrai contenu main disparaît de l'index FTS).
//!
//! ## Fix attendu (C4-1d)
//!
//! Clé d'identité composite `(vault_id, id)` → `research` obtient sa propre ligne + rowid → sa
//! propre entrée FTS scopée `research`. L'entrée FTS de `main` reste intacte. Collision impossible
//! par construction sur les couches rowid-dérivées (notes_fts, futures).
//!
//! ## Statut (TDD)
//!
//! ROUGE sous le code actuel (id-PK + sync FTS id-only) — VERT après le fix composite.

use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Construit une note `live` dans `vault` avec un ULID imposé et un corps donné.
fn note_in(vault: &str, id: NoteId, body: &str) -> Note {
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Live,
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

/// Un `research` collisionnant l'ULID d'une note live de `main` ne doit ni faire disparaître le
/// contenu de `main` de l'index FTS, ni y injecter le contenu `research`.
#[tokio::test]
async fn cross_vault_ulid_collision_does_not_clobber_main_fts() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let id = NoteId::new();

    // 1. Note live de `main` — mot unique « sphinx ».
    idx.upsert_note(&note_in("main", id, "main secret sphinx corpus"))
        .await
        .expect("upsert main");

    // 2. Note `research` avec le MÊME ULID — mot unique « gizmo ».
    idx.upsert_note(&note_in("research", id, "research payload gizmo corpus"))
        .await
        .expect("upsert research (ULID collisionné)");

    // 3. Recherche scopée `main` sur « sphinx » (contenu main) → DOIT être trouvée
    //    (l'entrée FTS de main n'a pas été clobbée).
    let main_sphinx = idx
        .search_fts_with_snippet(
            &VaultId::new("main"),
            "sphinx",
            10,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("search main sphinx");
    assert_eq!(
        main_sphinx.len(),
        1,
        "le contenu de main doit rester indexé FTS (sphinx trouvable dans main)"
    );

    // 4. Recherche scopée `main` sur « gizmo » (contenu research) → NE DOIT PAS fuiter dans main.
    let main_gizmo = idx
        .search_fts_with_snippet(
            &VaultId::new("main"),
            "gizmo",
            10,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("search main gizmo");
    assert!(
        main_gizmo.is_empty(),
        "le contenu de research NE DOIT PAS être searchable dans le vault main (FTS non clobbé)"
    );
}
