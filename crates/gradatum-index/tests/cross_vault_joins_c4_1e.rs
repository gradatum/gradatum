//! Isolation cross-vault des JOINs vers les tables filles par `note_id` (C4-1e, Slice E).
//!
//! Slice E scope TOUTE jointure enfant `note_id`-seul par `vault_id`, fermant la classe de
//! fuite cross-vault au flip (deux notes de MÊME ULID dans deux vaults distincts). Sites
//! couverts :
//!   1. `search_fts_with_snippet` — `LEFT JOIN temporal_index t ON … AND t.vault_id = n.vault_id`
//!   2. `timeline`                — `JOIN notes n ON … AND n.vault_id = t.vault_id`
//!   3. `search_semantic`         — `JOIN note_embeddings ne ON … AND ne.vault_id = n.vault_id`
//!   4. `audit_scan`              — idem embeddings (scan maintenance)
//!   5. `backfill_ann`            — idem embeddings (ré-insertion partition ANN)
//!   6. `get_override_raw`        — `AND vault_id = ?` (dérivation symétrique au write)
//!
//! Chaque site : un test « flag ON » (deux vaults, même ULID → la requête n'importe pas de
//! l'autre vault) + un test « flag OFF » (mono-vault → résultat byte-identical, la clause
//! `AND vault_id` est un no-op car `note_id` est unique).
//!
//! Régime multi-vault purement local au harnais (flag `multi_tenant.enabled` reste OFF) :
//! l'isolation est prouvée au niveau du stockage, indépendamment de la config serveur LIVE.

mod common;

use common::{VAULT_B, VAULT_MAIN, colliding_note_id, seed_colliding_note, two_vault_index};
use gradatum_core::VectorStore as _;
use gradatum_core::index::{AnchorSrc, TemporalEntry};
use gradatum_core::scope::{AclCheckedVaultId, OverrideScope, VaultId};
use gradatum_core::temporal_query::TimelineFilter;

/// Écrit une entrée `temporal_index` pour `(vault, note_id)` avec un `anchor_ms` donné.
///
/// Passe par le write-path public `write_temporal_entry` (INSERT OR REPLACE scopé
/// `(vault_id, note_id)` depuis la PK composite 0034) — deux vaults peuvent donc porter
/// une entrée temporelle de MÊME `note_id`.
async fn seed_temporal(
    idx: &gradatum_index::SqliteIndex,
    vault: &str,
    note_id: &str,
    anchor_ms: i64,
) {
    idx.write_temporal_entry(&TemporalEntry {
        note_id: note_id.to_string(),
        vault_id: vault.to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::Created,
        doc_kind: "Static".to_string(),
        valid_until_ms: None,
    })
    .await
    .expect("write_temporal_entry (seed test)");
}

// ── Site 1 : search_fts_with_snippet — LEFT JOIN temporal_index ────────────────

/// Flag ON : une entrée `temporal_index` du SEUL `vault-b` ne doit PAS enrichir l'`anchor_ms`
/// de la note homonyme de `main` via le `LEFT JOIN`.
///
/// RED avant Slice E : `LEFT JOIN temporal_index t ON t.note_id = n.id` (sans `t.vault_id =
/// n.vault_id`) attachait l'ancre de `vault-b` à la note de `main` → `anchor_ms = Some(T_b)`.
#[tokio::test]
async fn fts_temporal_join_on_no_cross_vault_anchor() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01FTSON").to_string();

    seed_colliding_note(&idx, VAULT_MAIN, "01FTSON", "titre-main").await;
    seed_colliding_note(&idx, VAULT_B, "01FTSON", "titre-b").await;
    // Ancre temporelle présente UNIQUEMENT dans vault-b.
    seed_temporal(&idx, VAULT_B, &nid, 1_700_000_000_000).await;

    let hits = idx
        .search_fts_with_snippet(
            &VaultId::new(VAULT_MAIN),
            "corps",
            10,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("search_fts_with_snippet main");

    let hit = hits
        .iter()
        .find(|h| h.note_id.to_string() == nid)
        .expect("la note de main doit matcher le FTS");
    assert_eq!(
        hit.anchor_ms, None,
        "main n'a pas d'entrée temporal_index : l'ancre de vault-b ne doit pas fuiter via le LEFT JOIN"
    );
}

/// Flag OFF (mono-vault) : la note de `main` avec son propre `temporal_index` conserve son
/// `anchor_ms` — la clause `AND t.vault_id = n.vault_id` est un no-op en mono-vault.
#[tokio::test]
async fn fts_temporal_join_off_byte_identical() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01FTSOFF").to_string();

    seed_colliding_note(&idx, VAULT_MAIN, "01FTSOFF", "titre-main").await;
    seed_temporal(&idx, VAULT_MAIN, &nid, 1_700_000_000_000).await;

    let hits = idx
        .search_fts_with_snippet(
            &VaultId::new(VAULT_MAIN),
            "corps",
            10,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("search_fts_with_snippet main");

    let hit = hits
        .iter()
        .find(|h| h.note_id.to_string() == nid)
        .expect("la note de main doit matcher le FTS");
    assert_eq!(
        hit.anchor_ms,
        Some(1_700_000_000_000),
        "mono-vault : l'ancre propre de main doit être présente (comportement inchangé)"
    );
}

// ── Site 2 : timeline — JOIN notes ────────────────────────────────────────────

/// Flag ON : une entrée `temporal_index` de `main` ne doit joindre QUE la note de `main`, pas
/// la note homonyme de `vault-b` — sinon le `JOIN notes` produit une ligne fantôme.
///
/// RED avant Slice E : `JOIN notes n ON n.id = t.note_id` (sans `n.vault_id = t.vault_id`)
/// matchait la note de `main` ET celle de `vault-b` (même ULID) → 2 lignes retournées.
#[tokio::test]
async fn timeline_notes_join_on_single_vault_row() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01TLON").to_string();

    seed_colliding_note(&idx, VAULT_MAIN, "01TLON", "titre-main").await;
    seed_colliding_note(&idx, VAULT_B, "01TLON", "titre-b").await;
    // Ancre temporelle UNIQUEMENT dans main : la timeline de main ne doit voir qu'une ligne.
    seed_temporal(&idx, VAULT_MAIN, &nid, 1_700_000_000_000).await;

    let rows = idx
        .timeline(
            &VaultId::new(VAULT_MAIN),
            &TimelineFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("timeline main");

    let matching: Vec<_> = rows
        .iter()
        .filter(|r| r.note_id.to_string() == nid)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "le JOIN notes doit rester scopé : exactement 1 ligne pour l'ULID (pas de doublon cross-vault)"
    );
}

/// Flag OFF (mono-vault) : la timeline de `main` retourne l'unique ligne de sa note — la
/// clause `AND n.vault_id = t.vault_id` ne change rien en mono-vault.
#[tokio::test]
async fn timeline_notes_join_off_byte_identical() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01TLOFF").to_string();

    seed_colliding_note(&idx, VAULT_MAIN, "01TLOFF", "titre-main").await;
    seed_temporal(&idx, VAULT_MAIN, &nid, 1_700_000_000_000).await;

    let rows = idx
        .timeline(
            &VaultId::new(VAULT_MAIN),
            &TimelineFilter {
                limit: 100,
                ..Default::default()
            },
        )
        .await
        .expect("timeline main");

    let matching: Vec<_> = rows
        .iter()
        .filter(|r| r.note_id.to_string() == nid)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "mono-vault : exactement 1 ligne pour la note"
    );
}

// ── Site 3 : search_semantic — JOIN note_embeddings ───────────────────────────

/// Flag ON : un embedding présent UNIQUEMENT dans `vault-b` ne doit pas être visible depuis
/// une recherche sémantique de `main` (fuite de vecteur cross-vault).
///
/// RED avant Slice E : `JOIN note_embeddings ne ON n.id = ne.note_id` (sans `ne.vault_id =
/// n.vault_id`) chargeait le vecteur de `vault-b` pour la note de `main` → résultat non vide.
#[tokio::test]
async fn semantic_embeddings_join_on_no_cross_vault_vector() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01SEMON");
    let embedder = "bge-m3";
    let query = vec![1.0_f32, 0.0, 0.0, 0.0];

    seed_colliding_note(&idx, VAULT_MAIN, "01SEMON", "corps-main").await;
    seed_colliding_note(&idx, VAULT_B, "01SEMON", "corps-b").await;
    // Embedding UNIQUEMENT dans vault-b.
    idx.insert_note_embedding(VAULT_B, &note_id, embedder, 4, &query)
        .await
        .expect("insert embedding vault-b");

    let results = idx
        .search_semantic(
            &AclCheckedVaultId::for_system_task(VaultId::new(VAULT_MAIN)),
            embedder,
            &query,
            10,
            None,
        )
        .await
        .expect("search_semantic main");

    assert!(
        results.is_empty(),
        "main n'a aucun embedding : le vecteur de vault-b ne doit pas fuiter (résultats={results:?})"
    );
}

/// Flag OFF (mono-vault) : un embedding de `main` est bien trouvé par la recherche sémantique
/// de `main` — la clause `AND ne.vault_id = n.vault_id` est un no-op en mono-vault.
#[tokio::test]
async fn semantic_embeddings_join_off_byte_identical() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01SEMOFF");
    let embedder = "bge-m3";
    let query = vec![1.0_f32, 0.0, 0.0, 0.0];

    seed_colliding_note(&idx, VAULT_MAIN, "01SEMOFF", "corps-main").await;
    idx.insert_note_embedding(VAULT_MAIN, &note_id, embedder, 4, &query)
        .await
        .expect("insert embedding main");

    let results = idx
        .search_semantic(
            &AclCheckedVaultId::for_system_task(VaultId::new(VAULT_MAIN)),
            embedder,
            &query,
            10,
            None,
        )
        .await
        .expect("search_semantic main");

    assert_eq!(
        results.len(),
        1,
        "mono-vault : l'embedding de main doit être trouvé"
    );
    assert_eq!(
        results[0].0, note_id,
        "le résultat doit être la note de main"
    );
}

// ── Site 4 : audit_scan — JOIN note_embeddings ────────────────────────────────

/// Flag ON : le scan d'audit de `main` ne doit PAS rattacher à sa note l'embedding présent
/// dans `vault-b` (même ULID).
///
/// RED avant Slice E : le JOIN embeddings id-only faisait apparaître le vecteur de `vault-b`
/// dans `AuditScanRow.embedding` de la note de `main`.
#[tokio::test]
async fn audit_scan_embeddings_join_on_no_cross_vault_vector() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01AUDON");
    let nid = note_id.to_string();

    seed_colliding_note(&idx, VAULT_MAIN, "01AUDON", "corps-main").await;
    seed_colliding_note(&idx, VAULT_B, "01AUDON", "corps-b").await;
    idx.insert_note_embedding(VAULT_B, &note_id, "bge-m3", 4, &[1.0_f32, 0.0, 0.0, 0.0])
        .await
        .expect("insert embedding vault-b");

    let rows = idx
        .audit_scan_inner(VAULT_MAIN, 100)
        .await
        .expect("audit_scan main");

    let row = rows
        .iter()
        .find(|r| r.note_id == nid)
        .expect("la note de main doit être scannée");
    assert!(
        row.embedding.is_none(),
        "main n'a aucun embedding : le vecteur de vault-b ne doit pas être rattaché à sa note"
    );
}

/// Flag OFF (mono-vault) : le scan d'audit de `main` rattache bien l'embedding propre de sa
/// note — comportement inchangé.
#[tokio::test]
async fn audit_scan_embeddings_join_off_byte_identical() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01AUDOFF");
    let nid = note_id.to_string();

    seed_colliding_note(&idx, VAULT_MAIN, "01AUDOFF", "corps-main").await;
    idx.insert_note_embedding(VAULT_MAIN, &note_id, "bge-m3", 4, &[1.0_f32, 0.0, 0.0, 0.0])
        .await
        .expect("insert embedding main");

    let rows = idx
        .audit_scan_inner(VAULT_MAIN, 100)
        .await
        .expect("audit_scan main");

    let row = rows
        .iter()
        .find(|r| r.note_id == nid)
        .expect("la note de main doit être scannée");
    assert!(
        row.embedding.is_some(),
        "mono-vault : l'embedding propre de la note de main doit être rattaché"
    );
}

// ── Site 5 : backfill_ann — JOIN note_embeddings ──────────────────────────────

/// Flag ON : le backfill ANN ne doit produire qu'UNE ligne par embedding réel, même quand une
/// note homonyme existe dans un autre vault.
///
/// RED avant Slice E : `JOIN notes n ON n.id = ne.note_id` (sans `ne.vault_id = n.vault_id`)
/// faisait joindre chaque embedding aux DEUX notes homonymes → double comptage (ré-insertion
/// dans la mauvaise partition ANN). Le count retourné par `backfill_ann_index` reflète le
/// nombre de lignes du SELECT (indépendant de vec0 : la table ANN n'est pas requise pour
/// compter, cf. `backfill_ann_avec_embeddings_retourne_count`).
///
/// Dim = 1024 (BGE_M3) : le SELECT du backfill filtre `ne.dim = 1024`.
#[tokio::test]
async fn backfill_ann_embeddings_join_on_no_double_count() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01BFON");
    let embedder = "bge-m3";
    let vec_1024 = vec![0.5_f32; 1024];

    seed_colliding_note(&idx, VAULT_MAIN, "01BFON", "corps-main").await;
    seed_colliding_note(&idx, VAULT_B, "01BFON", "corps-b").await;
    // Un embedding réel par vault (PK composite 0033 → deux lignes distinctes).
    idx.insert_note_embedding(VAULT_MAIN, &note_id, embedder, 1024, &vec_1024)
        .await
        .expect("insert embedding main");
    idx.insert_note_embedding(VAULT_B, &note_id, embedder, 1024, &vec_1024)
        .await
        .expect("insert embedding vault-b");

    let count = idx.backfill_ann_index().await.expect("backfill_ann_index");
    assert_eq!(
        count, 2,
        "2 embeddings réels → exactement 2 lignes de backfill (pas de double-count cross-vault)"
    );
}

/// Flag OFF (mono-vault) : un seul embedding → un seul backfill. La clause `AND ne.vault_id =
/// n.vault_id` ne change rien en mono-vault.
#[tokio::test]
async fn backfill_ann_embeddings_join_off_byte_identical() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01BFOFF");
    let vec_1024 = vec![0.5_f32; 1024];

    seed_colliding_note(&idx, VAULT_MAIN, "01BFOFF", "corps-main").await;
    idx.insert_note_embedding(VAULT_MAIN, &note_id, "bge-m3", 1024, &vec_1024)
        .await
        .expect("insert embedding main");

    let count = idx.backfill_ann_index().await.expect("backfill_ann_index");
    assert_eq!(count, 1, "mono-vault : 1 embedding → 1 ligne de backfill");
}

// ── Site 6 : get_override_raw — WHERE vault_id ────────────────────────────────

/// Flag ON : deux overlays de scope `Vault` sur la MÊME note (un par vault) restent lisibles
/// séparément — chaque lecture retourne le payload de SON vault.
///
/// Le `vault_id` de la lecture est dérivé À L'IDENTIQUE du write (`Vault(v)` → `vault_id = v`),
/// fermant la classe de mismatch read/write introduite par la PK composite 0034. Note : pour
/// le scope `Vault`, `scope_id ≡ vault_id`, donc l'isolation tient déjà par `scope_id` ; la
/// clause `AND vault_id = ?` garantit la symétrie read/write (propagation complète) et couvre
/// défensivement toute ligne dont le `vault_id` divergerait. La dérivation `"_unset"` pour
/// `Locus`/`Bearer` reste dormante (follow-up épic).
#[tokio::test]
async fn override_read_on_isolates_per_vault() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01OVRON");

    seed_colliding_note(&idx, VAULT_MAIN, "01OVRON", "corps-main").await;
    seed_colliding_note(&idx, VAULT_B, "01OVRON", "corps-b").await;

    let scope_main = OverrideScope::Vault(VaultId::new(VAULT_MAIN));
    let scope_b = OverrideScope::Vault(VaultId::new(VAULT_B));

    idx.upsert_override_raw(note_id, &scope_main, "trust", 1, "payload = \"main\"")
        .await
        .expect("upsert override main");
    idx.upsert_override_raw(note_id, &scope_b, "trust", 1, "payload = \"vault-b\"")
        .await
        .expect("upsert override vault-b");

    let got_main = idx
        .get_override_raw(note_id, &scope_main, "trust")
        .await
        .expect("get override main")
        .expect("override main présent");
    let got_b = idx
        .get_override_raw(note_id, &scope_b, "trust")
        .await
        .expect("get override vault-b")
        .expect("override vault-b présent");

    assert_eq!(
        got_main.1, "payload = \"main\"",
        "la lecture main doit voir SON payload"
    );
    assert_eq!(
        got_b.1, "payload = \"vault-b\"",
        "la lecture vault-b doit voir SON payload"
    );
}

/// Flag OFF (mono-vault) : write puis read d'un override de scope `Vault("main")` retourne le
/// payload — la clause `AND vault_id = ?` (dérivé `= scope_id = "main"`) est un no-op.
#[tokio::test]
async fn override_read_off_byte_identical() {
    let idx = two_vault_index().await;
    let note_id = colliding_note_id("01OVROFF");

    seed_colliding_note(&idx, VAULT_MAIN, "01OVROFF", "corps-main").await;

    let scope_main = OverrideScope::Vault(VaultId::new(VAULT_MAIN));
    idx.upsert_override_raw(note_id, &scope_main, "trust", 1, "payload = \"main\"")
        .await
        .expect("upsert override main");

    let got = idx
        .get_override_raw(note_id, &scope_main, "trust")
        .await
        .expect("get override main")
        .expect("override main présent");
    assert_eq!(
        got.1, "payload = \"main\"",
        "mono-vault : la lecture retourne le payload écrit"
    );
}
