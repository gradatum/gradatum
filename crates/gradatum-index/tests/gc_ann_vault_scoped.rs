//! GC des vecteurs ANN orphelins **scopé par vault**.
//!
//! `SqliteIndex::gc_orphan_ann(vault_id)` ne doit supprimer QUE les vecteurs orphelins de
//! la partition `vault_id` demandée : `WHERE vault_id=?1 AND note_id NOT IN (SELECT id FROM
//! notes WHERE vault_id=?1)`. Un GC ciblant `vault-b` ne touche donc AUCUNE partition `main`.
//!
//! Les tests exercent la méthode **inherent** `SqliteIndex::gc_orphan_ann(&str)` (parité avec
//! les tests de cascade D2 qui appellent `delete_note_from_index("vault-b", …)`). La couche
//! trait `Index::gc_orphan_ann(&VaultId)` n'est qu'une délégation `.as_str()`.
//!
//! ## Pourquoi une table plate « shadow » plutôt que la vraie table vec0
//!
//! `note_embeddings_ann` est une virtual table `vec0` (extension sqlite-vec) : bin-only
//! (`sqlite3_auto_extension`, `gradatum-server`) — indisponible dans les tests
//! `gradatum-index` (parité `ann_routing` T5.6 / `cascade_ann_preserves_other_vault_vector`).
//! Sans l'extension, la migration 0020 est skippée et la table est absente → le GC retourne
//! `Ok(0)` (mode dégradé) et le prédicat de partition ne peut PAS être exercé.
//!
//! La correction est **purement dans la clause `WHERE` du DELETE** : elle est donc
//! fidèlement testable sur une table plate portant les mêmes colonnes `(note_id, vault_id)`,
//! semée par [`SqliteIndex::seed_orphan_ann_for_test`]. Les quirks vec0 (PARTITION KEY sur
//! UPDATE) ne concernent pas un DELETE et sont hors périmètre GC. La fidélité vec0 réelle est
//! couverte à part par le test `#[ignore]` gaté vec0 en fin de fichier.

mod common;

use common::{VAULT_B, VAULT_MAIN, two_vault_index};
use gradatum_index::SqliteIndex;
use ulid::Ulid;

/// Compte les lignes ANN d'une partition `(vault_id, note_id)` via le helper de lecture
/// existant (fonctionne sur la table plate shadow comme sur la vraie table vec0).
async fn ann_count(idx: &SqliteIndex, vault_id: &str, note_id: &str) -> u64 {
    idx.count_child_rows_for_test("note_embeddings_ann", vault_id, note_id)
        .await
        .expect("count_child_rows_for_test note_embeddings_ann (table shadow présente)")
}

/// Régime multi-vault (flag ON, local au test) : `gc_orphan_ann("vault-b")` supprime UNIQUEMENT
/// l'orphelin ANN de `vault-b` et laisse INTACTS l'orphelin ANN de `main` **et** le vecteur
/// vivant de `main`.
///
/// RED (avant le correctif) : le DELETE global `WHERE note_id NOT IN (SELECT id FROM notes)`
/// scanne TOUTES les partitions → un `gc("vault-b")` effacerait aussi l'orphelin `main`,
/// faisant échouer l'assertion « l'orphelin `main` survit ».
#[tokio::test]
async fn gc_orphan_ann_scoped_preserves_other_vault() {
    let idx = two_vault_index().await;

    // Orphelin ANN dans vault-b : aucune note ne porte cet ULID → candidat GC de vault-b.
    let orphan_b = Ulid::new().to_string();
    idx.seed_orphan_ann_for_test(&orphan_b, VAULT_B, "bge-m3")
        .await
        .expect("seed orphan ANN vault-b");

    // Orphelin ANN dans main : aucune note ne porte cet ULID → candidat GC de main SEULEMENT.
    let orphan_main = Ulid::new().to_string();
    idx.seed_orphan_ann_for_test(&orphan_main, VAULT_MAIN, "bge-m3")
        .await
        .expect("seed orphan ANN main");

    // Vecteur VIVANT dans main : la note existe → jamais orphelin, jamais supprimé.
    let live_main = Ulid::new().to_string();
    idx.seed_note_with_fts(&live_main, "reference", "note vivante main")
        .await
        .expect("seed note vivante main");
    idx.seed_orphan_ann_for_test(&live_main, VAULT_MAIN, "bge-m3")
        .await
        .expect("seed vecteur vivant main");

    // GC scopé sur vault-b (méthode inherent, &str).
    let removed = idx
        .gc_orphan_ann(VAULT_B)
        .await
        .expect("gc_orphan_ann vault-b");

    assert_eq!(
        removed, 1,
        "gc(vault-b) doit supprimer exactement l'unique orphelin de vault-b (obtenu {removed})"
    );
    assert_eq!(
        ann_count(&idx, VAULT_B, &orphan_b).await,
        0,
        "l'orphelin ANN de vault-b doit être supprimé par gc(vault-b)"
    );
    // Garde comportementale vs DELETE global : l'orphelin de `main` NE doit PAS disparaître.
    assert_eq!(
        ann_count(&idx, VAULT_MAIN, &orphan_main).await,
        1,
        "l'orphelin ANN de `main` doit SURVIVRE à un gc ciblant `vault-b` (isolation partition)"
    );
    assert_eq!(
        ann_count(&idx, VAULT_MAIN, &live_main).await,
        1,
        "le vecteur vivant de `main` ne doit jamais être supprimé (note présente)"
    );

    // GC scopé sur main : purge son propre orphelin, préserve son vecteur vivant.
    let removed_main = idx
        .gc_orphan_ann(VAULT_MAIN)
        .await
        .expect("gc_orphan_ann main");
    assert_eq!(
        removed_main, 1,
        "gc(main) doit supprimer exactement l'orphelin de main (obtenu {removed_main})"
    );
    assert_eq!(
        ann_count(&idx, VAULT_MAIN, &orphan_main).await,
        0,
        "l'orphelin ANN de `main` doit être supprimé par son propre gc(main)"
    );
    assert_eq!(
        ann_count(&idx, VAULT_MAIN, &live_main).await,
        1,
        "le vecteur vivant de `main` survit à gc(main) (note présente)"
    );
}

/// Régime mono-vault (byte-identical flag OFF) : avec une seule partition `main`, l'ensemble
/// des orphelins supprimés par `gc_orphan_ann("main")` est identique à celui du GC global
/// historique — l'orphelin part, le vivant reste.
#[tokio::test]
async fn gc_orphan_ann_mono_vault_identical_orphan_set() {
    let idx = two_vault_index().await;

    let orphan = Ulid::new().to_string();
    idx.seed_orphan_ann_for_test(&orphan, VAULT_MAIN, "bge-m3")
        .await
        .expect("seed orphan main");

    let live = Ulid::new().to_string();
    idx.seed_note_with_fts(&live, "reference", "note vivante")
        .await
        .expect("seed note vivante");
    idx.seed_orphan_ann_for_test(&live, VAULT_MAIN, "bge-m3")
        .await
        .expect("seed vecteur vivant");

    let removed = idx
        .gc_orphan_ann(VAULT_MAIN)
        .await
        .expect("gc_orphan_ann main");

    assert_eq!(
        removed, 1,
        "mono-vault : exactement 1 orphelin supprimé (identique au GC global) — obtenu {removed}"
    );
    assert_eq!(
        ann_count(&idx, VAULT_MAIN, &orphan).await,
        0,
        "l'orphelin mono-vault doit être supprimé"
    );
    assert_eq!(
        ann_count(&idx, VAULT_MAIN, &live).await,
        1,
        "le vecteur vivant mono-vault doit rester"
    );
}

/// Mode dégradé (table ANN absente, cas LIVE 2026-07-12) : `gc_orphan_ann(vault)` retourne
/// `Ok(0)` sans erreur, pour n'importe quel vault — non-régression du no-op dégradé.
#[tokio::test]
async fn gc_orphan_ann_degraded_mode_scoped_returns_zero() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    for vault in [VAULT_MAIN, VAULT_B] {
        let removed = idx
            .gc_orphan_ann(vault)
            .await
            .unwrap_or_else(|e| panic!("gc dégradé {vault} : {e}"));
        assert_eq!(removed, 0, "table ANN absente → 0 orphelin ({vault})");
    }
}

/// Fidélité vec0 réelle (complément du test table-plate) : sur la VRAIE virtual table
/// `note_embeddings_ann` (vec0), un `gc_orphan_ann("vault-b")` ne détruit pas le vecteur ANN
/// vivant de `main` au même ULID (PK vec0 globale → un seul vecteur par ULID, ici partition
/// `main`).
///
/// GATÉ vec0 : l'extension sqlite-vec est bin-only — indisponible dans les tests
/// `gradatum-index`. Sans elle la table ANN est absente et le test s'auto-ignore (parité
/// `cascade_ann_preserves_other_vault_vector`). Lancement : extension enregistrée AVANT
/// `open_in_memory`, puis `-- --ignored`.
#[tokio::test]
#[ignore = "requiert l'extension sqlite-vec (vec0) enregistrée avant open_in_memory — bin-only"]
async fn gc_orphan_ann_scoped_vec0_real_table() {
    use common::{colliding_note_id, seed_colliding_note};
    use gradatum_core::VectorStore as _;

    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01T17ANN");
    let nid = note_id.to_string();

    seed_colliding_note(&idx, VAULT_MAIN, "01T17ANN", "corps-main").await;

    // Probe vec0 : sans l'extension, `note_embeddings_ann` est absente → auto-skip honnête.
    if idx
        .count_child_rows_for_test("note_embeddings_ann", VAULT_MAIN, &nid)
        .await
        .is_err()
    {
        eprintln!(
            "gc_orphan_ann_scoped_vec0_real_table : table ANN absente (vec0 non enregistré) — test ignoré"
        );
        return;
    }

    // Vecteur vivant de `main` via le write-path public (ligne ANN partition main).
    idx.insert_note_embedding(VAULT_MAIN, &note_id, "bge-m3", 4, &[0.1_f32, 0.2, 0.3, 0.4])
        .await
        .expect("insert embedding main (chemin ANN)");
    assert_eq!(
        ann_count(&idx, VAULT_MAIN, &nid).await,
        1,
        "main doit porter 1 vecteur ANN après son insert"
    );

    // GC ciblant `vault-b` (partition absente) : ne touche pas la partition `main`.
    let removed = idx.gc_orphan_ann(VAULT_B).await.expect("gc vault-b (vec0)");
    assert_eq!(
        removed, 0,
        "gc(vault-b) ne supprime rien (aucune partition vault-b)"
    );
    assert_eq!(
        ann_count(&idx, VAULT_MAIN, &nid).await,
        1,
        "le vecteur ANN vivant de `main` doit survivre à gc(vault-b)"
    );
}
