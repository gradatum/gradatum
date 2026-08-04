//! GATE DE SÉCURITÉ FORMELLE FUZZÉE — `test_no_cross_vault_leak`.
//!
//! Preuve, pour chaque **surface per-vault** de l'index partagé, qu'une écriture/lecture
//! d'un vault ne fuit JAMAIS vers un autre — sous un régime d'**ULID collisionnés** (même
//! `note_id`, `vault_id` distinct) entre `main` et `vault-b`. C'est la gate
//! obligatoire préalable à l'activation multi-vault : `multi_tenant` ne peut être activé
//! tant qu'elle n'est pas verte.
//!
//! ## Modèle du harnais (registre de handles)
//!
//! Un unique [`SqliteIndex`] partagé ([`two_vault_index`]) porte les deux vaults ; le
//! `vault_id` (colonne / PK composite) EST la frontière d'isolation — exactement le contrat
//! du registre `Map<vault_id, Vault>`, qui route N handles sur cet index unique.
//! Le régime « multi-vault » est **purement local au harnais** : le flag serveur
//! `multi_tenant.enabled` reste OFF en prod (flip INTERDIT). Les jeux mono-vault (`main`)
//! restent byte-identical.
//!
//! ## Fuzzing déterministe (reproductible CI)
//!
//! Seed FIXE (`FUZZ_SEED`) + splitmix64 local → séquence d'ULID/slug identique run-to-run
//! (aucun `rand`, aucune horloge dans la dérivation). Chaque test rejoue `FUZZ_ITERATIONS`
//! collisions et asserte l'isolation de SA surface. Un échec pointe l'ULID fautif exact.
//!
//! ## Classification EXHAUSTIVE des surfaces (aucune n'est oubliée)
//!
//! ### A. Surfaces per-vault PROUVÉES isolées ici (index-level)
//! - `notes` (PK composite `(vault_id,id)`, 0032) — [`notes_get_note_no_cross_vault_leak`]
//! - `notes_fts` — [`fts_search_no_cross_vault_leak`] (sync FTS scopée `(id,vault_id)`)
//! - `temporal_index` (0034 + delete scopé) — [`temporal_index_no_cross_vault_leak`]
//! - `note_overrides` scope `Vault` — [`overrides_vault_scope_no_cross_vault_leak`]
//! - `note_overrides` scope `Locus` (fin de la sentinelle `_unset`) —
//!   [`overrides_locus_scope_no_cross_vault_leak`]
//! - `redirect_table` (0035, PK composite + delete scopé) — [`redirect_table_no_cross_vault_leak`]
//! - `archive_index` (0028/0037 : lecture + tampering GC/restore) —
//!   [`archive_index_no_cross_vault_leak`]
//! - Cascade filles `note_index` / `note_overrides` / `note_links` / `note_audit_trail` /
//!   `note_embeddings` / `note_history` (delete scopé `(note_id,vault_id)`) —
//!   [`cascade_delete_child_tables_no_cross_vault_leak`]
//! - Coexistence sans clobber des mêmes tables filles (PK composite 0033/0034) —
//!   [`child_tables_coexist_no_cross_vault_clobber`]
//! - `note_embeddings_ann` GC scopée par partition —
//!   [`gc_orphan_ann_no_cross_vault_leak`]
//!
//! ### B. Surface embeddings-ANN INSERT-path — CLASSE FERMÉE (migration 0038)
//! `note_embeddings_ann` (vec0) était clé sur une **PK GLOBALE `note_id`** (schéma 0020) →
//! deux vaults au même ULID s'évinçaient (INSERT-path non scopable). La migration 0038
//! recompose la table en PARTITION KEY natif `(vault_id, embedder_id)` + colonne `note_id` :
//! le même ULID **coexiste** sur deux vaults, et `upsert_ann` bascule sur un upsert scopé
//! `(vault_id, note_id)` (DELETE+INSERT, sans éviction cross-vault). Prouvé par
//! [`ann_two_vaults_same_ulid_coexist`] (coexistence) et
//! [`ann_insert_path_composite_scoped_a4_closed`] (unicité composite intra-partition).
//! La GC (DELETE d'orphelins) reste scopée par partition (couverte en A). Fidélité
//! vec0 réelle en `#[ignore = "requiert libvec0"]` (vec0 bin-only, absent des tests index).
//!
//! ### C. Surfaces HORS-SCOPE isolation-vault — per-PRINCIPAL légitime (TenantId)
//! `note_usage`, `session_trace`, `event_log`, `proactive_surface`,
//! `proactive_recall_sessions`, `proactive_recall_feedback`, `read_usage_counters` :
//! keyées par le **principal** (`tenant_id`), PAS par le namespace vault. Ce sont des
//! données du principal, pas du vault → **pas une surface d'isolation vault**. Documentées
//! ici NOMMÉMENT ; leur isolation par principal relève d'un autre axe (non-goal ici).
//!
//! ### D. Surfaces GLOBALES légitimes — HORS-SCOPE (documenté)
//! `tenants`, `tenant_vault_grants`, `_schema_migrations`, `jobs*`, `metric_sample`,
//! `file_checksums`, `provider_status`, `request_log`, `api_keys`, `revoked` :
//! tables globales/infra, aucune colonne `vault_id`, aucune notion de fuite cross-vault.
//!
//! ### D-bis. Surfaces PER-CODE-VAULT — scopées, orthogonales au flip note-vault (documenté)
//! `code_vault` (PK `vault_id`) et `code_freshness` (PK `(vault_id, source_path)`) portent
//! bien `vault_id` — ce sont des surfaces **per-vault**, PAS globales (correction de revue
//! de sécurité : ne PAS les masquer en §D). MAIS elles vivent dans un **namespace
//! distinct `code-*`** (préfixe imposé, `sqlite.rs`), et leur clé est un **repo path / nom
//! de code-vault, PAS un ULID de note** → le modèle d'attaque « ULID collisionné » de cette
//! gate (multi-tenancy NOTE-vault) ne s'y applique pas. Les 8 chemins read/write sont
//! scopés `WHERE vault_id = ?1` (vérifié). Hors-scope de CETTE gate (frontière note-vault),
//! isolation propre garantie par la PK ; à couvrir par une gate code-vault dédiée si besoin.
//!
//! ### E. Flip-blockers ADMIN / server-level — DIFFÉRÉS (carte pré-flip), confinés `main`
//! - `read_note_by_id` split-brain : routage par handle effectif — couvert par
//!   le test dédié `read_back_vault_scoped.rs` (server) ; l'invariant index sous-jacent
//!   (`get_note(vault_id,id)` scopé) est prouvé en A.
//! - cache moka `EffectiveNoteCache` : caches per-instance → pas de fuite à flag
//!   OFF ; clé partitionnée `(VaultId,NoteId,u64)` — couvert par `cache_key_vault_partition.rs`.
//! - TOCTOU purge : re-check scopé — couvert par `purge_toctou_vault_scoped.rs`.
//! - `admin.rs get_active_archive`/purge/restore + GC worker + 5 handlers
//!   `const TENANT="main"` (jobs_v2/system/dashboard/review/project_map) : confinés `main`,
//!   PAS des fuites → différés (carte pré-flip), hors-gate.
//! - `search_fts_for_forget` FTS-driven (perf-sargable) : no-leak déjà prouvé ailleurs,
//!   perf ouverte → hors-gate (perf, pas leak).
//!
//! Toute FUITE résiduelle observée par cette gate = flip-blocker à REMONTER (jamais masquer).

mod common;

use common::{VAULT_B, VAULT_MAIN, colliding_note_id, seed_colliding_note, two_vault_index};
use gradatum_core::index::{AnchorSrc, TemporalEntry};
use gradatum_core::scope::{LocusId, OverrideScope, VaultId};
use gradatum_index::{ArchiveEntry, SqliteIndex};

/// Seed FIXE de la gate — toute modification change la séquence fuzzée (reproductibilité CI).
const FUZZ_SEED: u64 = 0xC41E_5EED_0BAD_F00D;

/// Nombre de collisions rejouées par surface. Assez large pour balayer des ULID variés,
/// assez borné pour rester sous la seconde en base in-memory.
const FUZZ_ITERATIONS: usize = 128;

/// PRNG déterministe (splitmix64) — reproductible, sans dépendance `rand` ni horloge.
///
/// Choisi pour la gate : un seed fixe rejoue exactement la même séquence d'ULID/slug à
/// chaque exécution CI (exigence « seed FIXE reproductible »).
struct DetRng(u64);

impl DetRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Tire le prochain `u64` pseudo-aléatoire déterministe (splitmix64).
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Clé de collision hex (16 chars, `[0-9a-f]`) — sûre en token FTS et en slug.
    ///
    /// La même clé, injectée dans `main` et `vault-b`, produit le MÊME ULID
    /// ([`colliding_note_id`]) : c'est la collision volontaire cross-vault.
    fn next_key(&mut self) -> String {
        format!("{:016x}", self.next_u64())
    }
}

/// Construit une [`TemporalEntry`] minimale pour un `(note_id, vault_id, anchor_ms)`.
fn temporal_entry(note_id: &str, vault_id: &str, anchor_ms: i64) -> TemporalEntry {
    TemporalEntry {
        note_id: note_id.to_string(),
        vault_id: vault_id.to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::Created,
        doc_kind: "Static".to_string(),
        valid_until_ms: None,
    }
}

/// Construit une entrée d'archive active, avec un `archive_path`/titre distinctifs par vault.
fn active_archive(note_id: &str, vault_id: &str, key: &str) -> ArchiveEntry {
    ArchiveEntry {
        note_id: note_id.to_string(),
        vault_id: vault_id.to_string(),
        section: "reference".to_string(),
        title: Some(format!("secret-{vault_id}-{key}")),
        original_locus: None,
        archive_path: format!(".archive/{vault_id}/{note_id}.md"),
        archived_at: 1_000,
        archived_by: Some(format!("admin-{vault_id}")),
        gc_due: 61_000,
        gc_at: None,
        restored_at: None,
    }
}

/// Compte les lignes filles scopées `(vault_id, note_id)` via le helper de test.
async fn child_count(idx: &SqliteIndex, table: &str, vault_id: &str, note_id: &str) -> u64 {
    idx.count_child_rows_for_test(table, vault_id, note_id)
        .await
        .unwrap_or_else(|e| panic!("count_child_rows_for_test {table} ({vault_id}) : {e}"))
}

/// Tables filles porteuses de `vault_id` couvertes par la cascade scopée `delete_note_from_index`.
const CHILD_TABLES: &[&str] = &[
    "note_index",
    "note_overrides",
    "note_links",
    "note_audit_trail",
    "note_embeddings",
    "note_history",
];

// ─────────────────────────────────────────────────────────────────────────────
// A. Surfaces per-vault PROUVÉES isolées
// ─────────────────────────────────────────────────────────────────────────────

/// `notes` / `get_note` — la lecture scopée d'un vault ne renvoie jamais le corps de l'autre,
/// même sous ULID collisionné (PK composite `(vault_id, id)`, migration 0032).
#[tokio::test]
async fn notes_get_note_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let ulid = colliding_note_id(&key).to_string();
        let marker_main = format!("mainmark{key}");
        let marker_b = format!("vaultbmark{key}");

        seed_colliding_note(&idx, VAULT_MAIN, &key, &marker_main).await;
        seed_colliding_note(&idx, VAULT_B, &key, &marker_b).await;

        let rec_main = idx
            .get_note(VAULT_MAIN, &ulid)
            .await
            .expect("get_note main")
            .expect("note `main` présente");
        let rec_b = idx
            .get_note(VAULT_B, &ulid)
            .await
            .expect("get_note vault-b")
            .expect("note `vault-b` présente");

        assert_eq!(
            rec_main.vault_id, VAULT_MAIN,
            "vault_id de la lecture `main`"
        );
        assert_eq!(rec_b.vault_id, VAULT_B, "vault_id de la lecture `vault-b`");
        assert!(
            rec_main.body_text.contains(&marker_main) && !rec_main.body_text.contains(&marker_b),
            "get_note(main) doit renvoyer le corps de `main`, jamais celui de `vault-b` (ULID {ulid})"
        );
        assert!(
            rec_b.body_text.contains(&marker_b) && !rec_b.body_text.contains(&marker_main),
            "get_note(vault-b) doit renvoyer le corps de `vault-b`, jamais celui de `main` (ULID {ulid})"
        );
    }
}

/// `notes_fts` / `search_fts_scored` — un terme présent uniquement dans un vault n'est jamais
/// résolu depuis l'autre vault (synchronisation FTS scopée `(id, vault_id)`, C4-1d).
#[tokio::test]
async fn fts_search_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let ulid = colliding_note_id(&key).to_string();
        let token_main = format!("mainmark{key}");
        let token_b = format!("vaultbmark{key}");

        // Écriture via `upsert_note` (chemin réel, FTS scopée par rowid `(id, vault_id)`).
        seed_colliding_note(&idx, VAULT_MAIN, &key, &token_main).await;
        seed_colliding_note(&idx, VAULT_B, &key, &token_b).await;

        // Le terme de `main` se résout dans `main`…
        let hits_main = idx
            .search_fts_scored(&VaultId::new(VAULT_MAIN), &token_main, 16, true)
            .await
            .expect("search_fts_scored main");
        assert!(
            hits_main.iter().any(|(nid, _, _)| nid.to_string() == ulid),
            "le terme de `main` doit être trouvé DANS `main` (ULID {ulid})"
        );
        // …mais JAMAIS depuis `vault-b`.
        let cross_b = idx
            .search_fts_scored(&VaultId::new(VAULT_B), &token_main, 16, true)
            .await
            .expect("search_fts_scored vault-b avec terme main");
        assert!(
            !cross_b.iter().any(|(nid, _, _)| nid.to_string() == ulid),
            "le terme de `main` ne doit JAMAIS être résolu depuis `vault-b` (ULID {ulid})"
        );

        // Symétrique : le terme de `vault-b` se résout dans `vault-b`, jamais dans `main`.
        let hits_b = idx
            .search_fts_scored(&VaultId::new(VAULT_B), &token_b, 16, true)
            .await
            .expect("search_fts_scored vault-b");
        assert!(
            hits_b.iter().any(|(nid, _, _)| nid.to_string() == ulid),
            "le terme de `vault-b` doit être trouvé DANS `vault-b` (ULID {ulid})"
        );
        let cross_main = idx
            .search_fts_scored(&VaultId::new(VAULT_MAIN), &token_b, 16, true)
            .await
            .expect("search_fts_scored main avec terme vault-b");
        assert!(
            !cross_main.iter().any(|(nid, _, _)| nid.to_string() == ulid),
            "le terme de `vault-b` ne doit JAMAIS être résolu depuis `main` (ULID {ulid})"
        );
    }
}

/// `temporal_index` — write scopé PK composite (0034) + delete scopé : le write ni le
/// delete d'un vault ne touchent l'entrée temporelle homonyme de l'autre.
#[tokio::test]
async fn temporal_index_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let ulid = colliding_note_id(&key).to_string();

        idx.write_temporal_entry(&temporal_entry(&ulid, VAULT_MAIN, 1_000))
            .await
            .expect("write temporal main");
        idx.write_temporal_entry(&temporal_entry(&ulid, VAULT_B, 2_000))
            .await
            .expect("write temporal vault-b");

        // Write : coexistence sans clobber (2 lignes distinctes).
        assert_eq!(
            child_count(&idx, "temporal_index", VAULT_MAIN, &ulid).await,
            1,
            "l'entrée temporelle de `main` ne doit pas être clobberée par `vault-b` (ULID {ulid})"
        );
        assert_eq!(
            child_count(&idx, "temporal_index", VAULT_B, &ulid).await,
            1,
            "`vault-b` doit porter sa propre entrée temporelle (ULID {ulid})"
        );

        // Delete scopé : supprimer `vault-b` ne touche pas `main`.
        assert!(
            idx.delete_temporal_entry(VAULT_B, &ulid)
                .await
                .expect("delete temporal vault-b"),
            "le delete de l'entrée `vault-b` doit réussir (ULID {ulid})"
        );
        assert_eq!(
            child_count(&idx, "temporal_index", VAULT_MAIN, &ulid).await,
            1,
            "l'entrée temporelle de `main` doit SURVIVRE au delete de `vault-b` (ULID {ulid})"
        );
        assert_eq!(
            child_count(&idx, "temporal_index", VAULT_B, &ulid).await,
            0,
            "l'entrée temporelle de `vault-b` doit avoir été supprimée (ULID {ulid})"
        );
    }
}

/// `note_overrides` scope `Vault` — read/write scopés : l'override d'un vault n'est jamais lu
/// avec le payload de l'autre.
#[tokio::test]
async fn overrides_vault_scope_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let note_id = colliding_note_id(&key);
        let scope_main = OverrideScope::Vault(VaultId::new(VAULT_MAIN));
        let scope_b = OverrideScope::Vault(VaultId::new(VAULT_B));

        idx.upsert_override_raw(
            note_id,
            &scope_main,
            "trust",
            1,
            &format!("owner = \"main-{key}\"\n"),
        )
        .await
        .expect("upsert override main");
        idx.upsert_override_raw(
            note_id,
            &scope_b,
            "trust",
            1,
            &format!("owner = \"vault-b-{key}\"\n"),
        )
        .await
        .expect("upsert override vault-b");

        let (_, payload_main) = idx
            .get_override_raw(note_id, &scope_main, "trust")
            .await
            .expect("get override main")
            .expect("override `main` présent");
        let (_, payload_b) = idx
            .get_override_raw(note_id, &scope_b, "trust")
            .await
            .expect("get override vault-b")
            .expect("override `vault-b` présent");

        assert!(
            payload_main.contains(&format!("main-{key}")) && !payload_main.contains("vault-b"),
            "l'override `main` doit conserver son payload, jamais celui de `vault-b` (key {key})"
        );
        assert!(
            payload_b.contains(&format!("vault-b-{key}")) && !payload_b.contains("\"main-"),
            "l'override `vault-b` doit conserver son payload, jamais celui de `main` (key {key})"
        );
    }
}

/// `note_overrides` scope `Locus` (model-change enum, fin de la sentinelle globale
/// `_unset`) : deux vaults écrivant un override Locus au MÊME `(note_id, locus)` ne se
/// clobberent plus. C'est LA surface qui échouait la gate avant ce correctif.
#[tokio::test]
async fn overrides_locus_scope_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let note_id = colliding_note_id(&key);
        // MÊME locus dans les deux vaults — collision maximale (ex-bucket `_unset`).
        let locus = LocusId::new("knowledge/shared");
        let scope_main = OverrideScope::Locus {
            vault: VaultId::new(VAULT_MAIN),
            locus: locus.clone(),
        };
        let scope_b = OverrideScope::Locus {
            vault: VaultId::new(VAULT_B),
            locus,
        };

        idx.upsert_override_raw(
            note_id,
            &scope_main,
            "trust",
            1,
            &format!("locus = \"main-{key}\"\n"),
        )
        .await
        .expect("upsert locus override main");
        idx.upsert_override_raw(
            note_id,
            &scope_b,
            "trust",
            1,
            &format!("locus = \"vault-b-{key}\"\n"),
        )
        .await
        .expect("upsert locus override vault-b");

        let (_, payload_main) = idx
            .get_override_raw(note_id, &scope_main, "trust")
            .await
            .expect("get locus override main")
            .expect("locus override `main` présent");
        let (_, payload_b) = idx
            .get_override_raw(note_id, &scope_b, "trust")
            .await
            .expect("get locus override vault-b")
            .expect("locus override `vault-b` présent");

        assert!(
            payload_main.contains(&format!("main-{key}")) && !payload_main.contains("vault-b"),
            "l'override Locus de `main` doit rester scopé `main`, jamais clobberé par `vault-b` (key {key})"
        );
        assert!(
            payload_b.contains(&format!("vault-b-{key}")) && !payload_b.contains("\"main-"),
            "l'override Locus de `vault-b` doit rester scopé `vault-b`, jamais clobberé par `main` (key {key})"
        );
    }
}

/// `redirect_table` (PK composite `(vault_id, title_slug)`, 0035) : un même slug
/// résout un ULID DIFFÉRENT par vault ; le delete scopé d'un vault ne touche pas l'autre.
#[tokio::test]
async fn redirect_table_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let slug = format!("slug-{key}");
        // Même slug, cibles ULID distinctes par vault.
        let ulid_main = colliding_note_id(&format!("{key}RA")).0;
        let ulid_b = colliding_note_id(&format!("{key}RB")).0;

        idx.upsert_redirect(VAULT_MAIN, &slug, &ulid_main, 1_000)
            .await
            .expect("upsert redirect main");
        idx.upsert_redirect(VAULT_B, &slug, &ulid_b, 2_000)
            .await
            .expect("upsert redirect vault-b");

        assert_eq!(
            idx.lookup_redirect(VAULT_MAIN, &slug)
                .await
                .expect("lookup main"),
            Some(ulid_main),
            "le slug de `main` doit résoudre la cible de `main`, jamais celle de `vault-b` (slug {slug})"
        );
        assert_eq!(
            idx.lookup_redirect(VAULT_B, &slug)
                .await
                .expect("lookup vault-b"),
            Some(ulid_b),
            "le slug de `vault-b` doit résoudre la cible de `vault-b`, jamais celle de `main` (slug {slug})"
        );

        // Delete scopé : retirer le redirect de `vault-b` laisse celui de `main` intact.
        idx.delete_redirect_by_ulid(VAULT_B, &ulid_b.to_string())
            .await
            .expect("delete redirect vault-b");
        assert_eq!(
            idx.lookup_redirect(VAULT_MAIN, &slug)
                .await
                .expect("lookup main post-delete"),
            Some(ulid_main),
            "le redirect de `main` doit survivre au delete de `vault-b` (slug {slug})"
        );
        assert_eq!(
            idx.lookup_redirect(VAULT_B, &slug)
                .await
                .expect("lookup vault-b post-delete"),
            None,
            "le redirect de `vault-b` doit avoir été supprimé (slug {slug})"
        );
    }
}

/// `archive_index` (info disclosure + tampering, la surface la plus grave) : la
/// lecture d'archive est scopée, et un GC/restore ciblant un vault n'affecte jamais l'archive
/// active homonyme de l'autre.
#[tokio::test]
async fn archive_index_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let ulid = colliding_note_id(&key).to_string();

        idx.insert_archive_entry(&active_archive(&ulid, VAULT_MAIN, &key))
            .await
            .expect("insert archive main");
        idx.insert_archive_entry(&active_archive(&ulid, VAULT_B, &key))
            .await
            .expect("insert archive vault-b");

        // Lecture scopée : chacun voit SON archive, jamais le corps de l'autre.
        let a_main = idx
            .get_active_archive(VAULT_MAIN, &ulid)
            .await
            .expect("get archive main")
            .expect("archive `main` active");
        let a_b = idx
            .get_active_archive(VAULT_B, &ulid)
            .await
            .expect("get archive vault-b")
            .expect("archive `vault-b` active");
        assert_eq!(a_main.vault_id, VAULT_MAIN);
        assert_eq!(
            a_main.title.as_deref(),
            Some(format!("secret-main-{key}").as_str())
        );
        assert_eq!(a_b.vault_id, VAULT_B);
        assert_eq!(
            a_b.title.as_deref(),
            Some(format!("secret-vault-b-{key}").as_str())
        );

        // Tampering : marquer `vault-b` détruite ne touche pas l'archive active de `main`.
        assert!(
            idx.mark_archive_gc(VAULT_B, &ulid, 99_000)
                .await
                .expect("mark_archive_gc vault-b"),
            "le GC de l'archive `vault-b` doit réussir (ULID {ulid})"
        );
        assert!(
            idx.get_active_archive(VAULT_MAIN, &ulid)
                .await
                .expect("get archive main post-gc")
                .is_some(),
            "l'archive active de `main` doit SURVIVRE au GC de `vault-b` (ULID {ulid})"
        );
        assert!(
            idx.get_active_archive(VAULT_B, &ulid)
                .await
                .expect("get archive vault-b post-gc")
                .is_none(),
            "l'archive de `vault-b` doit être détruite (plus active) (ULID {ulid})"
        );
    }
}

/// Cascade `delete_note_from_index` — supprimer une note d'un vault purge SES lignes filles
/// (`note_index`/`note_overrides`/`note_links`/`note_audit_trail`/`note_embeddings`/
/// `note_history`) scopées `(note_id, vault_id)`, sans jamais toucher celles de l'autre vault.
#[tokio::test]
async fn cascade_delete_child_tables_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let ulid = colliding_note_id(&key).to_string();

        // Notes réelles (rowid requis par le delete) + lignes filles dans les deux vaults.
        seed_colliding_note(&idx, VAULT_MAIN, &key, &format!("m{key}")).await;
        seed_colliding_note(&idx, VAULT_B, &key, &format!("b{key}")).await;
        for &table in CHILD_TABLES {
            idx.seed_child_row_for_test(table, VAULT_MAIN, &ulid)
                .await
                .unwrap_or_else(|e| panic!("seed {table} main : {e}"));
            idx.seed_child_row_for_test(table, VAULT_B, &ulid)
                .await
                .unwrap_or_else(|e| panic!("seed {table} vault-b : {e}"));
        }

        // Supprimer la note de `vault-b`.
        assert!(
            idx.delete_note_from_index(VAULT_B, &ulid)
                .await
                .expect("delete note vault-b"),
            "le delete de la note `vault-b` doit réussir (ULID {ulid})"
        );

        // Toutes les lignes filles de `main` survivent ; celles de `vault-b` sont purgées.
        for &table in CHILD_TABLES {
            assert_eq!(
                child_count(&idx, table, VAULT_MAIN, &ulid).await,
                1,
                "la ligne `{table}` de `main` doit SURVIVRE au delete de `vault-b` (ULID {ulid})"
            );
            assert_eq!(
                child_count(&idx, table, VAULT_B, &ulid).await,
                0,
                "la ligne `{table}` de `vault-b` doit avoir été purgée (ULID {ulid})"
            );
        }
        // La note `main` elle-même survit.
        assert!(
            idx.get_note(VAULT_MAIN, &ulid)
                .await
                .expect("get_note main")
                .is_some(),
            "la note `main` doit survivre au delete de `vault-b` (ULID {ulid})"
        );
    }
}

/// Coexistence sans clobber — deux vaults peuvent porter une ligne fille de MÊME `note_id`
/// dans chaque table (PK composite 0033/0034) : ni l'INSERT de l'un ne remplace celui de
/// l'autre, ni un `count` scopé ne fuit vers l'autre partition.
#[tokio::test]
async fn child_tables_coexist_no_cross_vault_clobber() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        let ulid = colliding_note_id(&key).to_string();

        // Notes parentes de MÊME ULID (une par vault) — requises depuis la migration 0039 :
        // les tables D2 (`note_audit_trail`/`note_embeddings`/`note_history`) portent un FK
        // composite `(vault_id, note_id) → notes(vault_id, id)`, un enfant orphelin est rejeté.
        seed_colliding_note(&idx, VAULT_MAIN, &key, &format!("m{key}")).await;
        seed_colliding_note(&idx, VAULT_B, &key, &format!("b{key}")).await;

        for &table in CHILD_TABLES {
            idx.seed_child_row_for_test(table, VAULT_MAIN, &ulid)
                .await
                .unwrap_or_else(|e| panic!("seed {table} main : {e}"));
            idx.seed_child_row_for_test(table, VAULT_B, &ulid)
                .await
                .unwrap_or_else(|e| panic!("seed {table} vault-b : {e}"));

            assert_eq!(
                child_count(&idx, table, VAULT_MAIN, &ulid).await,
                1,
                "`{table}` : la ligne de `main` ne doit pas être clobberée par `vault-b` (ULID {ulid})"
            );
            assert_eq!(
                child_count(&idx, table, VAULT_B, &ulid).await,
                1,
                "`{table}` : `vault-b` doit porter sa propre ligne (ULID {ulid})"
            );
        }
    }
}

/// `gc_orphan_ann` scopée par partition — le GC des orphelins ANN d'un vault ne
/// supprime jamais l'orphelin d'un autre vault (`WHERE vault_id = ?1 AND note_id NOT IN
/// (SELECT id FROM notes WHERE vault_id = ?1)`).
#[tokio::test]
async fn gc_orphan_ann_no_cross_vault_leak() {
    let idx = two_vault_index().await;
    let mut rng = DetRng::new(FUZZ_SEED);

    for _ in 0..FUZZ_ITERATIONS {
        let key = rng.next_key();
        // ULID DISTINCTS par vault : le GC scopé se prouve sur des orphelins disjoints, un par
        // partition. La coexistence d'un MÊME ULID sur deux vaults (possible depuis 0038, qui
        // a retiré la PK globale sur `note_id`) est couverte par le test
        // [`ann_two_vaults_same_ulid_coexist`] — hors scope ici.
        let ulid_main = colliding_note_id(&format!("{key}AM")).to_string();
        let ulid_b = colliding_note_id(&format!("{key}AB")).to_string();

        idx.seed_orphan_ann_for_test(&ulid_main, VAULT_MAIN, "test-emb")
            .await
            .expect("seed orphan ann main");
        idx.seed_orphan_ann_for_test(&ulid_b, VAULT_B, "test-emb")
            .await
            .expect("seed orphan ann vault-b");

        // GC de `vault-b` : ne doit toucher QUE la partition `vault-b`.
        idx.gc_orphan_ann(VAULT_B)
            .await
            .expect("gc_orphan_ann vault-b");

        assert_eq!(
            child_count(&idx, "note_embeddings_ann", VAULT_MAIN, &ulid_main).await,
            1,
            "le GC de `vault-b` ne doit PAS supprimer l'orphelin ANN de `main` (ULID {ulid_main})"
        );
        assert_eq!(
            child_count(&idx, "note_embeddings_ann", VAULT_B, &ulid_b).await,
            0,
            "le GC de `vault-b` doit supprimer son propre orphelin ANN (ULID {ulid_b})"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// B. Surface embeddings-ANN INSERT-path — HORS-SCOPE, prouvée par assertion (pas de skip)
// ─────────────────────────────────────────────────────────────────────────────

/// `note_embeddings_ann` (vec0) INSERT-path — classe FERMÉE (identité composite).
///
/// Avant cette recomposition, `note_id` était PRIMARY KEY GLOBAL (une seule ligne ANN par ULID, toutes
/// partitions confondues) → l'INSERT-path n'était pas scopable et deux vaults au même ULID
/// s'évinçaient. La migration 0038 recompose la table en PARTITION KEY `(vault_id, embedder_id)`
/// + colonne `note_id` : le même ULID **coexiste** sur deux vaults (prouvé par
/// [`ann_two_vaults_same_ulid_coexist`]). Ce test asserte la propriété COMPLÉMENTAIRE — au sein
/// d'une même partition `(vault_id, embedder_id)`, le triplet avec `note_id` reste unique
/// (doublon rejeté). La coexistence de deux embedders d'un même vault, elle, est requise et
/// prouvée côté crate (`sqlite_vec::tests::upsert_ann_conserve_une_ligne_par_embedder`).
/// La GC ANN reste scopée par partition (test A [`gc_orphan_ann_no_cross_vault_leak`]).
#[tokio::test]
async fn ann_insert_path_composite_scoped_a4_closed() {
    let idx = two_vault_index().await;
    let ulid = colliding_note_id("anncomposite").to_string();

    idx.seed_orphan_ann_for_test(&ulid, VAULT_MAIN, "test-emb")
        .await
        .expect("1er INSERT ANN (main) doit réussir");

    // Doublon dans la MÊME partition `(main, ULID)` → rejeté par la clé composite
    // `(vault_id, note_id)` (une seule ligne ANN par couple).
    let dup = idx
        .seed_orphan_ann_for_test(&ulid, VAULT_MAIN, "test-emb")
        .await;
    assert!(
        dup.is_err(),
        "un doublon `(vault_id, note_id)` dans la même partition doit être rejeté \
         (identité composite unique)"
    );
}

/// Même ULID indexé dans DEUX vaults (ANN) → LES DEUX vecteurs coexistent.
///
/// Preuve de la fermeture de l'éviction cross-vault ANN (migration 0038 : PARTITION KEY
/// natif `(vault_id, embedder_id)`, `note_id` colonne ordinaire — plus de PK globale).
/// Sur table shadow (convention : vec0 bin-only, fidélité réelle en `#[ignore]`).
#[tokio::test]
async fn ann_two_vaults_same_ulid_coexist() {
    let idx = two_vault_index().await;
    let ulid = colliding_note_id("anncoexist").to_string();

    idx.seed_orphan_ann_for_test(&ulid, VAULT_MAIN, "test-emb")
        .await
        .expect("seed ann main");
    idx.seed_orphan_ann_for_test(&ulid, VAULT_B, "test-emb")
        .await
        .expect("seed ann vault-b (même ULID) — doit coexister (plus de PK globale)");

    assert_eq!(
        child_count(&idx, "note_embeddings_ann", VAULT_MAIN, &ulid).await,
        1,
        "le vecteur ANN de `main` doit être présent (ULID {ulid})"
    );
    assert_eq!(
        child_count(&idx, "note_embeddings_ann", VAULT_B, &ulid).await,
        1,
        "le vecteur ANN de `vault-b` (même ULID) doit coexister — pas d'éviction (ULID {ulid})"
    );
}

/// Une recherche ANN scopée à un vault ne renvoie JAMAIS le vecteur d'un autre vault,
/// même sous ULID collisionné (volet « search ne fuit pas », COMPLÉMENTAIRE de
/// [`ann_two_vaults_same_ulid_coexist`] qui ne prouve que la coexistence).
///
/// La recherche ANN réelle ([`gradatum_index::sqlite_vec::search_ann_inner`]) borne ses
/// résultats par `WHERE ann.vault_id = ?1` : une requête scopée à un vault ne peut donc pas
/// surfacer une ligne d'un autre vault. Ce test exerce cette clause sur la table shadow (vec0
/// bin-only, fidélité réelle en `#[ignore]`) via la lecture scopée `(vault_id, note_id)` :
///
/// 1. **collision** — même ULID X semé dans `main` ET `vault-b` : chaque partition ne voit que
///    SA ligne (1), aucune éviction (coexistence).
/// 2. **non-fuite** — un ULID Y semé UNIQUEMENT dans `vault-b` est INVISIBLE à une requête
///    scopée `main` (0) : la recherche de `main` ne retourne jamais le vecteur de `vault-b`.
#[tokio::test]
async fn ann_insert_collision_cross_vault() {
    let idx = two_vault_index().await;

    // (1) Collision : même ULID dans les deux vaults → coexistence scopée (pas d'éviction).
    let collided = colliding_note_id("anncollision").to_string();
    idx.seed_orphan_ann_for_test(&collided, VAULT_MAIN, "test-emb")
        .await
        .expect("seed ann main (ULID collisionné)");
    idx.seed_orphan_ann_for_test(&collided, VAULT_B, "test-emb")
        .await
        .expect("seed ann vault-b (même ULID) — doit coexister");

    assert_eq!(
        child_count(&idx, "note_embeddings_ann", VAULT_MAIN, &collided).await,
        1,
        "partition `main` : exactement son vecteur pour l'ULID collisionné {collided}"
    );
    assert_eq!(
        child_count(&idx, "note_embeddings_ann", VAULT_B, &collided).await,
        1,
        "partition `vault-b` : exactement son vecteur pour l'ULID collisionné {collided}"
    );

    // (2) Non-fuite : un vecteur présent UNIQUEMENT dans `vault-b` est invisible côté `main`.
    let only_b = colliding_note_id("annonlyb").to_string();
    idx.seed_orphan_ann_for_test(&only_b, VAULT_B, "test-emb")
        .await
        .expect("seed ann vault-b (exclusif)");

    assert_eq!(
        child_count(&idx, "note_embeddings_ann", VAULT_MAIN, &only_b).await,
        0,
        "une recherche ANN scopée `main` ne doit JAMAIS retourner le vecteur exclusif de \
         `vault-b` (ULID {only_b}) — clause `WHERE ann.vault_id = ?` de search_ann_inner"
    );
    assert_eq!(
        child_count(&idx, "note_embeddings_ann", VAULT_B, &only_b).await,
        1,
        "le vecteur exclusif de `vault-b` reste visible dans sa propre partition (ULID {only_b})"
    );
}
