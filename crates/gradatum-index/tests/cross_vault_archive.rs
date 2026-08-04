//! Scoping `vault_id` du registre `archive_index`.
//!
//! Trou réel (audit sécu, info-disclosure LE PLUS GRAVE) : `get_active_archive` /
//! `mark_archive_gc` / `mark_archive_restored` filtraient `WHERE note_id = ?` **id-only**
//! alors que la table porte `vault_id` (migration 0028). À ≥2 vaults, un ULID collisionné
//! laissait :
//!   - lire le corps archivé d'un AUTRE vault (fuite) ;
//!   - marquer GC / restauré la ligne d'un AUTRE vault (tampering).
//!
//! Contrainte de schéma exploitée par les scénarios : `uidx_archive_active` est un unique
//! partiel sur `note_id` SEUL (`WHERE gc_at IS NULL AND restored_at IS NULL`) — donc AU PLUS
//! une archive ACTIVE par ULID globalement. La fuite se manifeste donc quand UN seul vault
//! détient l'archive active et qu'un caller d'un AUTRE vault interroge le même ULID :
//! id-only → il obtient / mute la ligne du premier. Le fix ferme cette classe à la source
//! (défense en profondeur : les guards `entry.vault_id != …` du layer Vault restent).

use gradatum_index::{ArchiveEntry, ArchiveListFilter, SqliteIndex};

/// Entrée d'archive active de test, dans `vault_id`, avec un corps distinctif par vault.
fn active_entry(note_id: &str, vault_id: &str) -> ArchiveEntry {
    ArchiveEntry {
        note_id: note_id.to_string(),
        vault_id: vault_id.to_string(),
        section: format!("section-{vault_id}"),
        title: Some(format!("secret-{vault_id}")),
        original_locus: Some("projets".to_string()),
        archive_path: format!(".archive/{vault_id}/projets/{note_id}.md"),
        archived_at: 1000,
        archived_by: Some(format!("admin-{vault_id}")),
        gc_due: 61000,
        gc_at: None,
        restored_at: None,
    }
}

const X: &str = "01CROSSVAULTARCHIVE00000X";

/// PS-1 — info disclosure : `main` détient l'archive active de X, `vault-b` n'a rien.
/// Un caller `vault-b` NE DOIT PAS obtenir le corps archivé de `main`.
#[tokio::test]
async fn get_active_archive_is_scoped_by_vault_no_cross_read() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    idx.insert_archive_entry(&active_entry(X, "main"))
        .await
        .expect("insert main archive");

    // Le vault propriétaire résout sa propre archive (chemin nominal, byte-identical mono-vault).
    let own = idx
        .get_active_archive("main", X)
        .await
        .expect("get_active main")
        .expect("archive active de main présente");
    assert_eq!(own.vault_id, "main");
    assert_eq!(own.title.as_deref(), Some("secret-main"));

    // Un AUTRE vault ne voit RIEN pour ce même ULID (fuite fermée).
    let cross = idx
        .get_active_archive("vault-b", X)
        .await
        .expect("get_active vault-b");
    assert!(
        cross.is_none(),
        "fuite cross-vault : vault-b a lu l'archive de main pour l'ULID collisionné"
    );
}

/// PS-2 — tampering GC : `main` détient l'archive active de X. Un caller `vault-b`
/// NE DOIT PAS pouvoir marquer GC la ligne de `main`.
#[tokio::test]
async fn mark_archive_gc_is_scoped_by_vault_no_cross_clobber() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    idx.insert_archive_entry(&active_entry(X, "main"))
        .await
        .expect("insert main archive");

    // vault-b tente de GC l'ULID collisionné → aucune ligne lui appartenant → false.
    let marked = idx
        .mark_archive_gc("vault-b", X, 2000)
        .await
        .expect("mark_archive_gc vault-b");
    assert!(
        !marked,
        "tampering cross-vault : vault-b a marqué GC l'archive de main"
    );

    // L'archive de main est INTACTE (toujours active, gc_at non posé).
    let still = idx
        .get_active_archive("main", X)
        .await
        .expect("get_active main")
        .expect("archive de main toujours active");
    assert!(still.gc_at.is_none(), "gc_at de main ne doit pas être posé");

    // Le vault propriétaire, lui, marque bien GC (chemin nominal préservé).
    let own = idx
        .mark_archive_gc("main", X, 3000)
        .await
        .expect("mark_archive_gc main");
    assert!(
        own,
        "le vault propriétaire doit pouvoir GC sa propre archive"
    );
    assert!(
        idx.get_active_archive("main", X)
            .await
            .expect("get_active main post-gc")
            .is_none(),
        "après GC par le propriétaire, plus d'archive active"
    );
    // Trace préservée (la ligne survit).
    let with_gc = idx
        .list_archive_entries(&ArchiveListFilter {
            include_gc: true,
            ..Default::default()
        })
        .await
        .expect("list include_gc");
    assert_eq!(with_gc.len(), 1);
    assert_eq!(with_gc[0].gc_at, Some(3000));
}

/// PS-3 — tampering restore : `main` détient l'archive active de X. Un caller `vault-b`
/// NE DOIT PAS pouvoir marquer restaurée la ligne de `main`.
#[tokio::test]
async fn mark_archive_restored_is_scoped_by_vault_no_cross_clobber() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    idx.insert_archive_entry(&active_entry(X, "main"))
        .await
        .expect("insert main archive");

    let marked = idx
        .mark_archive_restored("vault-b", X, 2000)
        .await
        .expect("mark_archive_restored vault-b");
    assert!(
        !marked,
        "tampering cross-vault : vault-b a marqué restaurée l'archive de main"
    );

    let still = idx
        .get_active_archive("main", X)
        .await
        .expect("get_active main")
        .expect("archive de main toujours active");
    assert!(
        still.restored_at.is_none(),
        "restored_at de main ne doit pas être posé"
    );

    // Le vault propriétaire restaure bien sa propre archive.
    let own = idx
        .mark_archive_restored("main", X, 3000)
        .await
        .expect("mark_archive_restored main");
    assert!(
        own,
        "le vault propriétaire doit pouvoir restaurer sa propre archive"
    );
}
