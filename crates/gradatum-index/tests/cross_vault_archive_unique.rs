//! Recomposition de l'unique partiel `uidx_archive_active` en `(vault_id, note_id)`
//! (flip-blocker multi-vault).
//!
//! ## Défaut fermé (disponibilité/correction, PAS fuite)
//!
//! `uidx_archive_active` (migration 0028) est un unique partiel **GLOBAL sur `note_id`
//! seul** (`WHERE gc_at IS NULL AND restored_at IS NULL`). Au régime multi-vault, si
//! vault-A détient une archive ACTIVE de l'ULID X, alors vault-B archivant le MÊME ULID
//! X voit son `insert_archive_entry` rejeté par la contrainte d'unicité — un **DoS
//! cross-vault** : un vault peut empêcher l'archivage d'un ULID homonyme dans un autre
//! vault. Incohérent avec le PK composite `(vault_id, id)` de `notes` (migration 0032),
//! qui autorise justement deux notes de même ULID dans des vaults distincts.
//!
//! Ce n'est PAS une fuite (lectures et mutations déjà scopées par `vault_id`) —
//! c'est un défaut de disponibilité/correction multi-vault, masqué en mono-vault.
//!
//! ## Fix (migration 0037)
//!
//! L'unique partiel est recomposé sur `(vault_id, note_id)` : au plus UNE archive active
//! PAR VAULT et PAR ULID. En mono-vault `main`, `(main, note_id)` ≡ ancienne clé
//! `note_id` (au plus une archive active par ULID dans `main`) → comportement inchangé,
//! byte-identical flag OFF.

use gradatum_index::{ArchiveEntry, ArchiveListFilter, SqliteIndex};

/// Entrée d'archive active de test, dans `vault_id`, avec un corps distinctif par vault.
fn active_entry(note_id: &str, vault_id: &str) -> ArchiveEntry {
    ArchiveEntry {
        note_id: note_id.to_string(),
        vault_id: vault_id.to_string(),
        section: format!("section-{vault_id}"),
        title: Some(format!("title-{vault_id}")),
        original_locus: Some("projets".to_string()),
        archive_path: format!(".archive/{vault_id}/projets/{note_id}.md"),
        archived_at: 1000,
        archived_by: Some(format!("admin-{vault_id}")),
        gc_due: 61000,
        gc_at: None,
        restored_at: None,
    }
}

const X: &str = "01CROSSVAULTARCHIVEUNIQUEX";

/// PS-1 — deux vaults archivent le MÊME ULID X : les DEUX archives actives
/// sont acceptées (une par vault). Avant le fix (unique global sur `note_id`), le second
/// `insert_archive_entry` échouait = DoS cross-vault.
#[tokio::test]
async fn two_vaults_can_archive_same_ulid_concurrently() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // main archive X.
    idx.insert_archive_entry(&active_entry(X, "main"))
        .await
        .expect("insert archive active de main");

    // vault-b archive le MÊME ULID X → DOIT réussir (clé composite (vault_id, note_id)).
    idx.insert_archive_entry(&active_entry(X, "vault-b"))
        .await
        .expect(
            "DoS cross-vault : vault-b n'a pas pu archiver l'ULID X déjà archivé par main \
             (unique global sur note_id — attendu unique composite (vault_id, note_id))",
        );

    // Chaque vault résout SA propre archive active, distincte (scoping Task 20 préservé).
    let own_main = idx
        .get_active_archive("main", X)
        .await
        .expect("get_active main")
        .expect("archive active de main présente");
    assert_eq!(own_main.vault_id, "main");
    assert_eq!(own_main.title.as_deref(), Some("title-main"));

    let own_b = idx
        .get_active_archive("vault-b", X)
        .await
        .expect("get_active vault-b")
        .expect("archive active de vault-b présente");
    assert_eq!(own_b.vault_id, "vault-b");
    assert_eq!(own_b.title.as_deref(), Some("title-vault-b"));

    // Deux lignes actives (une par vault) coexistent pour le même ULID.
    let all_active = idx
        .list_archive_entries(&ArchiveListFilter::default())
        .await
        .expect("list active");
    let active_x: Vec<_> = all_active.iter().filter(|e| e.note_id == X).collect();
    assert_eq!(
        active_x.len(),
        2,
        "deux archives actives (main + vault-b) attendues pour l'ULID X, obtenu {}",
        active_x.len()
    );
}

/// PS-2 (byte-identical mono-vault) — l'invariant intra-vault est PRÉSERVÉ : un même
/// vault ne peut PAS détenir deux archives actives pour le même ULID. Après le fix, la
/// clé composite `(vault_id, note_id)` conserve exactement l'ancien comportement dans
/// `main` (au plus une archive active par ULID).
#[tokio::test]
async fn same_vault_still_rejects_second_active_archive_for_same_ulid() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    idx.insert_archive_entry(&active_entry(X, "main"))
        .await
        .expect("premier insert main");

    // Second insert actif pour (main, X) → DOIT être rejeté par l'unique partiel composite.
    let dup = idx.insert_archive_entry(&active_entry(X, "main")).await;
    assert!(
        dup.is_err(),
        "invariant intra-vault violé : (main, X) accepte deux archives actives — \
         l'unicité partielle (vault_id, note_id) doit rejeter le doublon"
    );

    // Une seule archive active dans main.
    let active = idx
        .list_archive_entries(&ArchiveListFilter::default())
        .await
        .expect("list active");
    let active_main_x: Vec<_> = active
        .iter()
        .filter(|e| e.note_id == X && e.vault_id == "main")
        .collect();
    assert_eq!(
        active_main_x.len(),
        1,
        "une seule archive active attendue pour (main, X), obtenu {}",
        active_main_x.len()
    );
}

/// PS-3 (non-interférence GC/restore) — après GC de l'archive de `main`, `main` peut
/// ré-archiver X (l'unique partiel ne compte que les lignes actives), sans jamais toucher
/// l'archive active de `vault-b`. Vérifie que la recomposition d'index conserve la
/// sémantique partielle `WHERE gc_at IS NULL AND restored_at IS NULL`.
#[tokio::test]
async fn partial_predicate_preserved_after_gc_per_vault() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    idx.insert_archive_entry(&active_entry(X, "main"))
        .await
        .expect("insert main");
    idx.insert_archive_entry(&active_entry(X, "vault-b"))
        .await
        .expect("insert vault-b");

    // main GC son archive → sa ligne quitte le prédicat partiel.
    let marked = idx
        .mark_archive_gc("main", X, 2000)
        .await
        .expect("mark_archive_gc main");
    assert!(marked, "main doit pouvoir GC sa propre archive");

    // main ré-archive X → autorisé (plus d'archive active pour (main, X)).
    idx.insert_archive_entry(&active_entry(X, "main"))
        .await
        .expect("ré-archivage de main après GC (prédicat partiel préservé)");

    // vault-b reste intact et actif tout du long.
    let b = idx
        .get_active_archive("vault-b", X)
        .await
        .expect("get_active vault-b")
        .expect("archive active de vault-b intacte");
    assert!(b.gc_at.is_none(), "gc_at de vault-b ne doit pas être posé");
    assert_eq!(b.title.as_deref(), Some("title-vault-b"));
}
