//! Tests F-100 incrément 1.6 — registre des archives (`archive_index`).
//!
//! Couvre le CRUD du registre qui pilote le GC de rétention : insertion, résolution
//! de l'archive active, listing filtré/paginé, sélection GC échue, marquage GC et
//! restauration (la ligne survit comme trace), invariant d'unicité de l'archive active.

use gradatum_index::{ArchiveEntry, ArchiveListFilter, SqliteIndex};

fn entry(note_id: &str, section: &str, archived_at: i64, gc_due: i64) -> ArchiveEntry {
    entry_in_vault(note_id, "main", section, archived_at, gc_due)
}

fn entry_in_vault(
    note_id: &str,
    vault_id: &str,
    section: &str,
    archived_at: i64,
    gc_due: i64,
) -> ArchiveEntry {
    ArchiveEntry {
        note_id: note_id.to_string(),
        vault_id: vault_id.to_string(),
        section: section.to_string(),
        title: Some(format!("Titre {note_id}")),
        original_locus: Some("projets".to_string()),
        archive_path: format!(".archive/{vault_id}/projets/{note_id}.md"),
        archived_at,
        archived_by: Some("test-admin".to_string()),
        gc_due,
        gc_at: None,
        restored_at: None,
    }
}

/// Insertion + résolution de l'archive active : tous les champs sont restitués.
#[tokio::test]
async fn insert_and_resolve_active_archive() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    let e = entry("01ARCHIVE0000000000000001", "council", 1000, 61000);
    idx.insert_archive_entry(&e).await.expect("insert");

    let got = idx
        .get_active_archive("main", "01ARCHIVE0000000000000001")
        .await
        .expect("get_active")
        .expect("archive active présente");
    assert_eq!(
        got, e,
        "l'entrée restituée doit être identique (archived_by inclus)"
    );
    assert_eq!(got.archived_by.as_deref(), Some("test-admin"));
    assert!(got.gc_at.is_none() && got.restored_at.is_none());
}

/// Listing par défaut : exclut GC et restaurées ; filtres section/date + pagination.
#[tokio::test]
async fn list_defaults_and_filters() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    idx.insert_archive_entry(&entry("01A0000000000000000000AAA", "feedback", 100, 60100))
        .await
        .expect("insert a");
    idx.insert_archive_entry(&entry("01A0000000000000000000BBB", "reference", 200, 60200))
        .await
        .expect("insert b");
    idx.insert_archive_entry(&entry("01A0000000000000000000CCC", "feedback", 300, 60300))
        .await
        .expect("insert c");

    // Défaut : les 3 actives, triées archived_at DESC.
    let all = idx
        .list_archive_entries(&ArchiveListFilter::default())
        .await
        .expect("list default");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].note_id, "01A0000000000000000000CCC");

    // Filtre section=feedback → 2.
    let fb = idx
        .list_archive_entries(&ArchiveListFilter {
            section: Some("feedback".to_string()),
            ..Default::default()
        })
        .await
        .expect("list feedback");
    assert_eq!(fb.len(), 2);
    assert!(fb.iter().all(|e| e.section == "feedback"));

    // Fenêtre temporelle archived_at ∈ [200, 300].
    let win = idx
        .list_archive_entries(&ArchiveListFilter {
            from_ms: Some(200),
            until_ms: Some(300),
            ..Default::default()
        })
        .await
        .expect("list window");
    assert_eq!(win.len(), 2);

    // Pagination limit=1 offset=1 → 2e plus récente (BBB).
    let page = idx
        .list_archive_entries(&ArchiveListFilter {
            limit: 1,
            offset: 1,
            ..Default::default()
        })
        .await
        .expect("list page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].note_id, "01A0000000000000000000BBB");
}

/// L'insert porte le `vault_id`, et le filtre `vault_id` restreint le listing
/// (anticipation multi-vault v1.0 ; le GC reste cross-vault).
#[tokio::test]
async fn insert_carries_vault_id_and_list_filters_by_vault() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    idx.insert_archive_entry(&entry_in_vault(
        "01VAULT00000000000000MAIN",
        "main",
        "feedback",
        100,
        60100,
    ))
    .await
    .expect("insert main");
    idx.insert_archive_entry(&entry_in_vault(
        "01VAULT0000000000000OTHER",
        "code-gradatum",
        "reference",
        200,
        60200,
    ))
    .await
    .expect("insert other");

    // Le vault_id est bien restitué à la résolution.
    let got = idx
        .get_active_archive("code-gradatum", "01VAULT0000000000000OTHER")
        .await
        .expect("get_active")
        .expect("archive active");
    assert_eq!(got.vault_id, "code-gradatum");

    // Sans filtre vault → les 2 vaults.
    let all = idx
        .list_archive_entries(&ArchiveListFilter::default())
        .await
        .expect("list all");
    assert_eq!(all.len(), 2, "les 2 vaults visibles sans filtre");

    // Filtre vault=main → 1 seule.
    let only_main = idx
        .list_archive_entries(&ArchiveListFilter {
            vault_id: Some("main".to_string()),
            ..Default::default()
        })
        .await
        .expect("list main");
    assert_eq!(only_main.len(), 1);
    assert_eq!(only_main[0].note_id, "01VAULT00000000000000MAIN");
}

/// GC échu : `select_gc_due_archives` ne retourne que gc_due < now ET actives.
#[tokio::test]
async fn select_gc_due_respects_deadline_and_active() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    // Échue (gc_due=500 < now=1000).
    idx.insert_archive_entry(&entry("01DUE00000000000000000001", "feedback", 100, 500))
        .await
        .expect("insert due");
    // Non échue (gc_due=5000 > now).
    idx.insert_archive_entry(&entry("01FRESH000000000000000001", "feedback", 100, 5000))
        .await
        .expect("insert fresh");

    let due = idx
        .select_gc_due_archives(1000, 100)
        .await
        .expect("select gc");
    assert_eq!(due.len(), 1, "seule l'archive échue est candidate");
    assert_eq!(due[0].note_id, "01DUE00000000000000000001");
}

/// Marquage GC : la ligne survit (invisible en actif, visible avec include_gc).
#[tokio::test]
async fn mark_gc_preserves_row_as_trace() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    let e = entry("01GC00000000000000000001A", "feedback", 100, 500);
    idx.insert_archive_entry(&e).await.expect("insert");

    let marked = idx
        .mark_archive_gc("main", "01GC00000000000000000001A", 2000)
        .await
        .expect("mark gc");
    assert!(marked, "1er marquage GC → true");

    // Plus active.
    assert!(
        idx.get_active_archive("main", "01GC00000000000000000001A")
            .await
            .expect("get_active")
            .is_none()
    );
    // Mais toujours présente avec include_gc.
    let with_gc = idx
        .list_archive_entries(&ArchiveListFilter {
            include_gc: true,
            ..Default::default()
        })
        .await
        .expect("list include_gc");
    assert_eq!(with_gc.len(), 1);
    assert_eq!(with_gc[0].gc_at, Some(2000));

    // Idempotent : 2e marquage → false (plus d'active).
    let again = idx
        .mark_archive_gc("main", "01GC00000000000000000001A", 3000)
        .await
        .expect("mark gc 2");
    assert!(!again, "2e marquage GC → false");
    // Plus candidate au GC.
    assert!(
        idx.select_gc_due_archives(9999, 100)
            .await
            .expect("select gc")
            .is_empty()
    );
}

/// Marquage restauration : la ligne survit, sort de l'actif, visible include_restored.
#[tokio::test]
async fn mark_restored_preserves_row_as_trace() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    let e = entry("01RES0000000000000000001A", "reference", 100, 60100);
    idx.insert_archive_entry(&e).await.expect("insert");

    let marked = idx
        .mark_archive_restored("main", "01RES0000000000000000001A", 2000)
        .await
        .expect("mark restored");
    assert!(marked);
    assert!(
        idx.get_active_archive("main", "01RES0000000000000000001A")
            .await
            .expect("get_active")
            .is_none()
    );
    let with_restored = idx
        .list_archive_entries(&ArchiveListFilter {
            include_restored: true,
            ..Default::default()
        })
        .await
        .expect("list include_restored");
    assert_eq!(with_restored.len(), 1);
    assert_eq!(with_restored[0].restored_at, Some(2000));
}

/// Invariant : au plus une archive ACTIVE par note (unique partiel). Une note
/// ré-archivée APRÈS restauration est acceptée (nouvelle ligne active).
#[tokio::test]
async fn active_uniqueness_then_rearchive_after_restore() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    let e = entry("01UNIQ000000000000000001A", "feedback", 100, 60100);
    idx.insert_archive_entry(&e).await.expect("insert 1");

    // Seconde archive active du même ULID → rejet (unique partiel).
    let dup = idx.insert_archive_entry(&e).await;
    assert!(
        dup.is_err(),
        "seconde archive active du même ULID doit échouer"
    );

    // Après restauration de la 1re, une nouvelle archive active est acceptée.
    idx.mark_archive_restored("main", "01UNIQ000000000000000001A", 2000)
        .await
        .expect("restore");
    let e2 = entry("01UNIQ000000000000000001A", "feedback", 3000, 63000);
    idx.insert_archive_entry(&e2)
        .await
        .expect("ré-archivage après restauration accepté");

    let active = idx
        .get_active_archive("main", "01UNIQ000000000000000001A")
        .await
        .expect("get_active")
        .expect("nouvelle archive active");
    assert_eq!(active.archived_at, 3000);
}
