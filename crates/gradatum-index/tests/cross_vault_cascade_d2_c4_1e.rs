//! Isolation cross-vault des 3 tables filles migrées en Slice D2 (C4-1e, migration 0033) :
//! `note_audit_trail`, `note_embeddings`, `note_history`. Ces tables ont reçu `vault_id`
//! (0033) et sont désormais purgées par la cascade **scopée** `(note_id, vault_id)` de
//! `delete_note_from_index` (D2.3), au même titre que les enfants D1.
//!
//! Complément du test D1 `cross_vault_cascade_delete_c4_1e.rs` (qui verrouille
//! `note_index`/`temporal_index`/`note_links`/`note_overrides`). Contrairement à
//! `note_index`/`temporal_index` (PK `note_id` seule → un seul vault possible), les 3
//! tables D2 acceptent deux lignes de MÊME `note_id` dans deux vaults distincts (PK
//! composite incluant `vault_id`, ou ligne-unique `id` pour l'audit) : le test ON les
//! sème donc dans les DEUX vaults et prouve qu'un `delete("vault-b")` ne touche que
//! `vault-b`.
//!
//! Écrit aussi la non-régression du write-path embeddings (`insert_note_embedding`) :
//! en régime ON, un embedding de `vault-b` ne clobbe pas celui de `main` partageant
//! `(note_id, embedder_id)` (unicité composite 0033) ; en OFF (mono-vault), le round-trip
//! reste byte-identical.
//!
//! Régime multi-vault purement local au harnais (flag `multi_tenant.enabled` reste OFF).

mod common;

use common::{colliding_note_id, seed_colliding_note, two_vault_index};
use gradatum_core::VectorStore as _;
use gradatum_index::SqliteIndex;

/// Les 3 tables filles migrées en D2 (0033), scopées `(note_id, vault_id)`.
const D2_CHILD_TABLES: [&str; 3] = ["note_audit_trail", "note_embeddings", "note_history"];

/// `true` si au moins une ligne fille scopée `(vault_id, note_id)` subsiste dans `table`.
async fn child_exists(idx: &SqliteIndex, table: &str, vault_id: &str, note_id: &str) -> bool {
    idx.count_child_rows_for_test(table, vault_id, note_id)
        .await
        .expect("count_child_rows_for_test (table D2 de la liste blanche)")
        > 0
}

/// Régime multi-vault (flag ON, local au test) : `delete_note_from_index("vault-b", id)`
/// ne doit toucher AUCUNE ligne fille D2 de `main` à même ULID, et supprimer celles de
/// `vault-b`.
///
/// RED avant D2.3 : les 3 tables étaient dans la boucle id-only → le delete `vault-b`
/// effaçait aussi les enfants de `main` (même note_id).
#[tokio::test]
async fn cascade_d2_preserves_other_vault_children() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01D2ON").to_string();

    // Deux notes parentes de MÊME ULID (une par vault) + les 3 enfants D2 dans CHAQUE vault.
    seed_colliding_note(&idx, "main", "01D2ON", "corps-main").await;
    seed_colliding_note(&idx, "vault-b", "01D2ON", "corps-b").await;
    for vault in ["main", "vault-b"] {
        for table in D2_CHILD_TABLES {
            idx.seed_child_row_for_test(table, vault, &nid)
                .await
                .unwrap_or_else(|e| panic!("seed enfant {table}/{vault} : {e}"));
        }
    }

    idx.delete_note_from_index("vault-b", &nid)
        .await
        .expect("delete_note_from_index vault-b");

    // Enfants D2 de `main` : TOUS subsistent (isolation cross-vault).
    for table in D2_CHILD_TABLES {
        assert!(
            child_exists(&idx, table, "main", &nid).await,
            "l'enfant D2 `{table}` de `main` ne doit PAS être supprimé par un delete ciblant `vault-b`"
        );
    }
    // Enfants D2 de `vault-b` : supprimés par la cascade scopée.
    for table in D2_CHILD_TABLES {
        assert!(
            !child_exists(&idx, table, "vault-b", &nid).await,
            "l'enfant D2 `{table}` de `vault-b` doit être supprimé par le delete ciblé"
        );
    }
}

/// Régime mono-vault (byte-identical flag OFF) : `delete_note_from_index` purge la note
/// ET ses 3 enfants D2 — 0 orphelin, comportement inchangé.
#[tokio::test]
async fn cascade_d2_single_vault_complete() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01D2OFF").to_string();

    seed_colliding_note(&idx, "main", "01D2OFF", "corps-off").await;
    for table in D2_CHILD_TABLES {
        idx.seed_child_row_for_test(table, "main", &nid)
            .await
            .unwrap_or_else(|e| panic!("seed enfant mono-vault {table} : {e}"));
    }

    let deleted = idx
        .delete_note_from_index("main", &nid)
        .await
        .expect("delete_note_from_index main");
    assert!(deleted, "la note existante doit être supprimée (Ok(true))");

    for table in D2_CHILD_TABLES {
        assert!(
            !child_exists(&idx, table, "main", &nid).await,
            "orphelin D2 détecté dans `{table}` après delete mono-vault"
        );
    }
}

/// Write-path embeddings, régime ON : `insert_note_embedding("vault-b", …)` crée une ligne
/// distincte et ne clobbe PAS l'embedding de `main` partageant `(note_id, embedder_id)`.
///
/// RED avant 0033 : PK `(note_id, embedder_id)` → l'insert `vault-b` faisait un
/// `ON CONFLICT DO UPDATE` sur l'unique ligne, écrasant l'embedding de `main`.
#[tokio::test]
async fn write_embedding_on_no_cross_vault_clobber() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01D2EMB");
    let nid = note_id.to_string();
    let embedder = "bge-m3";
    let vec_main = vec![0.1_f32, 0.2, 0.3, 0.4];
    let vec_b = vec![0.9_f32, 0.8, 0.7, 0.6];

    seed_colliding_note(&idx, "main", "01D2EMB", "corps-main").await;
    seed_colliding_note(&idx, "vault-b", "01D2EMB", "corps-b").await;

    idx.insert_note_embedding("main", &note_id, embedder, 4, &vec_main)
        .await
        .expect("insert embedding main");
    assert_eq!(
        idx.count_child_rows_for_test("note_embeddings", "main", &nid)
            .await
            .expect("count main"),
        1,
        "main doit porter exactement 1 embedding après son insert"
    );
    assert_eq!(
        idx.count_child_rows_for_test("note_embeddings", "vault-b", &nid)
            .await
            .expect("count vault-b"),
        0,
        "vault-b ne doit avoir aucun embedding avant son propre insert"
    );

    // Insert vault-b sur le MÊME (note_id, embedder_id) : ligne séparée (PK composite 0033).
    idx.insert_note_embedding("vault-b", &note_id, embedder, 4, &vec_b)
        .await
        .expect("insert embedding vault-b");

    assert_eq!(
        idx.count_child_rows_for_test("note_embeddings", "main", &nid)
            .await
            .expect("count main post"),
        1,
        "l'insert de `vault-b` ne doit PAS clobber la ligne de `main` (isolation composite)"
    );
    assert_eq!(
        idx.count_child_rows_for_test("note_embeddings", "vault-b", &nid)
            .await
            .expect("count vault-b post"),
        1,
        "`vault-b` doit porter sa propre ligne d'embedding"
    );
}

/// Isolation cross-vault de la cascade ANN (`note_embeddings_ann`, vec0) — complétion D2.
///
/// `note_embeddings_ann` porte `vault_id` en PARTITION KEY vec0 (migration 0020) ; le DELETE
/// de cascade `delete_note_from_index` est désormais scopé `(note_id, vault_id)`. Un
/// `delete("vault-b", X)` ne doit donc PAS détruire le vecteur ANN de `main` au même ULID.
/// Le PRIMARY KEY vec0 `note_id` est global (une seule ligne ANN par ULID) : on sème donc
/// UN seul vecteur (partition `main`) et on prouve qu'un delete ciblant `vault-b` (partition
/// absente) le laisse intact, tandis qu'un delete ciblant `main` le purge.
///
/// GATÉ vec0 : l'extension sqlite-vec est bin-only (`sqlite3_auto_extension`, gradatum-server)
/// — indisponible dans les tests `gradatum-index`. Sans elle, la migration 0020 est skippée,
/// la table ANN est absente et le test s'auto-ignore (parité `ann_routing` T5.6). Lancement :
/// extension enregistrée AVANT `open_in_memory`, puis `-- --ignored`.
#[tokio::test]
#[ignore = "requiert l'extension sqlite-vec (vec0) enregistrée avant open_in_memory — bin-only"]
async fn cascade_ann_preserves_other_vault_vector() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01D2ANN");
    let nid = note_id.to_string();

    seed_colliding_note(&idx, "main", "01D2ANN", "corps-main").await;

    // Probe vec0 : sans l'extension, `note_embeddings_ann` est absente → auto-skip honnête.
    if idx
        .count_child_rows_for_test("note_embeddings_ann", "main", &nid)
        .await
        .is_err()
    {
        eprintln!(
            "cascade_ann_preserves_other_vault_vector : table ANN absente (vec0 non enregistré) — test ignoré"
        );
        return;
    }

    // Seed d'un embedding pour `main` → ligne ANN (partition vault=main) via le write-path public.
    idx.insert_note_embedding("main", &note_id, "bge-m3", 4, &[0.1_f32, 0.2, 0.3, 0.4])
        .await
        .expect("insert embedding main (chemin ANN)");
    assert_eq!(
        idx.count_child_rows_for_test("note_embeddings_ann", "main", &nid)
            .await
            .expect("count ANN main"),
        1,
        "main doit porter exactement 1 vecteur ANN après son insert"
    );

    // Delete ciblant `vault-b` : le DELETE ANN scopé ne matche pas la partition `main`.
    idx.delete_note_from_index("vault-b", &nid)
        .await
        .expect("delete_note_from_index vault-b");
    assert_eq!(
        idx.count_child_rows_for_test("note_embeddings_ann", "main", &nid)
            .await
            .expect("count ANN main post delete vault-b"),
        1,
        "le vecteur ANN de `main` doit survivre à un delete ciblant `vault-b`"
    );

    // Delete ciblant `main` : purge effective de sa propre partition.
    idx.delete_note_from_index("main", &nid)
        .await
        .expect("delete_note_from_index main");
    assert_eq!(
        idx.count_child_rows_for_test("note_embeddings_ann", "main", &nid)
            .await
            .expect("count ANN main post delete main"),
        0,
        "le vecteur ANN de `main` doit être purgé par son propre delete"
    );
}

/// Write-path embeddings, régime OFF (mono-vault) : le round-trip insert → get retourne
/// le vecteur exact — le passage à la PK composite 0033 ne change rien en mono-vault.
#[tokio::test]
async fn write_embedding_off_roundtrip_byte_identical() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01D2OFFEMB");
    let embedder = "bge-m3";
    let vector = vec![0.11_f32, 0.22, 0.33, 0.44];

    seed_colliding_note(&idx, "main", "01D2OFFEMB", "corps").await;
    idx.insert_note_embedding("main", &note_id, embedder, 4, &vector)
        .await
        .expect("insert embedding main OFF");

    let got = idx
        .get_note_embedding("main", &note_id, embedder)
        .await
        .expect("get embedding")
        .expect("embedding présent");
    assert_eq!(
        got, vector,
        "round-trip embedding mono-vault non byte-identical"
    );
}

/// Isolation cross-vault de la JOIN `search_ann_inner` (vec0) — complétion « E-JOINs »
/// (C4-1e Slice E, `sqlite_vec.rs::search_ann_inner`).
///
/// Deux notes de MÊME ULID (`main` + `vault-b`, cf. [`colliding_note_id`]). Le PRIMARY KEY
/// vec0 `note_id` est GLOBAL (une seule ligne ANN par ULID, cf.
/// `cascade_ann_preserves_other_vault_vector` ci-dessus) : on sème donc UN seul vecteur,
/// côté `vault-b`.
///
/// Sans le prédicat `ann.vault_id = n.vault_id` de la JOIN, `n.id = ann.note_id` matche
/// INDIFFÉREMMENT les 2 lignes `notes` (main + vault-b partagent le même `id`) pour ce seul
/// vecteur ANN — produisant potentiellement 2 lignes candidates (dup) ET laissant le statut
/// `Live` de `main` masquer un `downgrade_note` fait côté `vault-b` (hijack cross-vault :
/// le filtre `n.status != 'downgraded'` est évalué sur la MAUVAISE ligne `notes`). Avec le
/// prédicat scopé (déjà présent dans `sqlite_vec.rs`), seule la ligne `notes` du vault
/// interrogé peut joindre son propre vecteur ANN.
///
/// Cas 1 (les deux notes `Live`) : la recherche ANN dans `vault-b` retourne exactement
/// 1 résultat — pas de duplication issue d'un double-match de la JOIN.
/// Cas 2 (`vault-b` explicitement `downgrade_note`e, `main` reste `Live`) : la recherche
/// ANN dans `vault-b` retourne 0 résultat — le statut `Live` de `main` ne doit PAS
/// ressusciter la note downgradée de `vault-b` (filtre respecté, pas de bypass via le
/// mauvais vault).
///
/// GATÉ vec0 : l'extension sqlite-vec est bin-only (`sqlite3_auto_extension`,
/// `gradatum-server`) — indisponible dans les tests `gradatum-index`. Sans elle, la table
/// ANN est absente et le test s'auto-ignore (parité `ann_routing` T5.6 /
/// `cascade_ann_preserves_other_vault_vector`). Lancement : extension enregistrée AVANT
/// `open_in_memory`, puis `-- --ignored`.
#[tokio::test]
#[ignore = "requiert l'extension sqlite-vec (vec0) enregistrée avant open_in_memory — bin-only"]
async fn search_ann_join_scoped_by_vault_no_status_bypass() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01EJOINS");
    let nid = note_id.to_string();
    let embedder = "bge-m3";
    let dim = 1024usize;
    let mut query = vec![0.0_f32; dim];
    query[0] = 1.0; // vecteur unité axe 0 — cosine 1.0 avec lui-même.

    seed_colliding_note(&idx, "main", "01EJOINS", "corps-main").await;
    seed_colliding_note(&idx, "vault-b", "01EJOINS", "corps-b").await;

    // Probe vec0 : sans l'extension, `note_embeddings_ann` est absente → auto-skip honnête.
    if idx
        .count_child_rows_for_test("note_embeddings_ann", "vault-b", &nid)
        .await
        .is_err()
    {
        eprintln!(
            "search_ann_join_scoped_by_vault_no_status_bypass : table ANN absente (vec0 non enregistré) — test ignoré"
        );
        return;
    }

    // Seed de l'embedding CÔTÉ vault-b uniquement (PK vec0 globale par note_id).
    idx.insert_note_embedding("vault-b", &note_id, embedder, dim as u16, &query)
        .await
        .expect("insert embedding vault-b");
    assert_eq!(
        idx.count_child_rows_for_test("note_embeddings_ann", "vault-b", &nid)
            .await
            .expect("count ANN vault-b"),
        1,
        "vault-b doit porter exactement 1 vecteur ANN après son insert"
    );

    idx.set_ann_enabled(true);
    let vault_b_checked = gradatum_core::scope::AclCheckedVaultId::for_system_task(
        gradatum_core::scope::VaultId::new("vault-b"),
    );

    // Phase 1 : les deux notes sont `Live` — pas de duplication du résultat.
    let results_live = idx
        .search_semantic(&vault_b_checked, embedder, &query, 5, None)
        .await
        .expect("search_semantic vault-b (ANN, phase Live)");
    assert_eq!(
        results_live.len(),
        1,
        "1 seul vecteur ANN existe pour cet ULID — la JOIN ne doit PAS le dupliquer via la \
         ligne `notes` de `main` (trouvé {} résultat(s))",
        results_live.len()
    );

    // Phase 2 : `vault-b` explicitement downgradée, `main` reste `Live`.
    idx.downgrade_note(
        &vault_b_checked,
        &note_id,
        "test E-JOINs : downgrade vault-b, main reste live",
        None,
    )
    .await
    .expect("downgrade_note vault-b");

    let results_downgraded = idx
        .search_semantic(&vault_b_checked, embedder, &query, 5, None)
        .await
        .expect("search_semantic vault-b (ANN, phase downgraded)");
    assert!(
        results_downgraded.is_empty(),
        "vault-b downgradée ne doit PAS réapparaître via le statut `Live` de `main` (hijack \
         cross-vault du JOIN non scopé) — trouvé {} résultat(s)",
        results_downgraded.len()
    );
}
