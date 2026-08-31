//! Tests d'intégration pour `review_promote::promote_once`.
//!
//! Couvre :
//! 1. `promote_once_promotes_aged_staging` — note staging âgée → promue, stats.staging=1.
//! 2. `promote_once_does_not_promote_recent` — note récente → reste staging, stats=0.
//! 3. `promote_once_disabled_skips_all` — `cfg.enabled=false` → aucune promotion.
//! 4. `promote_once_promotes_pending_review` — note pending-review âgée → métrique from_status.
//! 5. `promote_once_db_error_reflected_in_stats` — find_promotable échoue → stats.errors=1.
//!
//! Stratégie d'isolation :
//! - `Vault::create` (TempDir) = backend vault réel avec son propre `SqliteIndex` interne.
//! - `vault.index()` est passé comme `Arc<dyn Index>` — même base SQLite → `find_promotable`
//!   voit exactement ce qu'a écrit le vault.
//! - Pour simuler une note « âgée », on appelle `promote_once` avec `now_ms` = real_time + 20j,
//!   ce qui rend la note âgée relative au cutoff configuré (14j) sans bypasser l'index.
//! - `now_future` = real_time + 20 * 86_400_000 ms (20 jours dans le futur).
//! - `FailFindPromotableIndex` : mock minimal implémentant les 3 sous-traits de `Index`,
//!   retourne `Err` sur `find_promotable` pour simuler une panne DB.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::{ContentHash, NoteId};
use gradatum_core::index::{FileChecksumEntry, Index, NoteRecord};
use gradatum_core::index_store::{AuthorRow, LessonHitRaw, Lineage, SearchHitRaw};
use gradatum_core::note::Note;
use gradatum_core::scope::{AclCheckedVaultId, LocusId, OverrideScope, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::{DocumentStore, IndexStore, VectorStore};
use gradatum_server::{
    config::{ReviewPromoteConfig, ServerConfig},
    metrics::AppMetrics,
    review_promote::{PromoteStats, promote_once, promote_tick},
    state::VaultRegistry,
};
use gradatum_vault::{Registry, Vault};
use tempfile::TempDir;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Mock minimal — FailFindPromotableIndex
// ---------------------------------------------------------------------------
//
// Implémente les 3 sous-traits requis par le blanket impl `Index` avec des
// réponses vides/no-op, SAUF `find_promotable` qui retourne toujours `Err`.
// Utilisé uniquement pour tester que `promote_once` reporte l'erreur dans
// `stats.errors` au lieu de l'avaler silencieusement.

struct FailFindPromotableIndex;

#[async_trait]
impl DocumentStore for FailFindPromotableIndex {
    async fn write_note(&self, _note: &Note) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn get_content_hash(
        &self,
        _vault_id: &str,
        _id: NoteId,
    ) -> Result<Option<ContentHash>, GradatumError> {
        Ok(None)
    }
    async fn get_note(
        &self,
        _tenant_id: &str,
        _note_id_ulid: &str,
    ) -> Result<Option<NoteRecord>, GradatumError> {
        Ok(None)
    }
    async fn list_by_status(
        &self,
        _vault_id: &VaultId,
        _status: NoteStatus,
    ) -> Result<Vec<NoteId>, GradatumError> {
        Ok(vec![])
    }
    async fn downgrade_note(
        &self,
        _vault: &gradatum_core::scope::AclCheckedVaultId,
        _note_id: &NoteId,
        _reason: &str,
        _replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn patch_note_status(
        &self,
        _vault: &gradatum_core::scope::AclCheckedVaultId,
        _note_id: &NoteId,
        _status: Option<&str>,
        _status_reason: Option<&str>,
        _replaced_by: Option<&NoteId>,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn upsert_note_title(
        &self,
        _vault_id: &str,
        _note_id: &NoteId,
        _title: &str,
    ) -> Result<usize, GradatumError> {
        Ok(0)
    }
    async fn update_note_locus(
        &self,
        _vault: &gradatum_core::scope::AclCheckedVaultId,
        _note_id: &NoteId,
        _new_locus: &LocusId,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn mark_forgotten(
        &self,
        _vault_id: &str,
        _note_id: &str,
        _by: Option<&str>,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn reassert_forgotten(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn unmark_forgotten(&self, _vault_id: &str, _note_id: &str) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn list_forgotten(
        &self,
        _vault_id: &str,
        _limit: usize,
        _cursor: Option<&str>,
    ) -> Result<Vec<(String, Option<String>, String, i64, Option<String>)>, GradatumError> {
        Ok(vec![])
    }
    async fn count_forgotten(&self, _vault_id: &str) -> Result<usize, GradatumError> {
        Ok(0)
    }
    async fn count_notes_by_status(
        &self,
        _vault_id: &str,
    ) -> Result<HashMap<String, u64>, GradatumError> {
        Ok(HashMap::new())
    }
}

#[async_trait]
impl IndexStore for FailFindPromotableIndex {
    async fn search_fts(
        &self,
        _vault_id: &VaultId,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteId>, GradatumError> {
        Ok(vec![])
    }
    async fn search_fts_scored(
        &self,
        _vault_id: &VaultId,
        _query: &str,
        _limit: usize,
        _include_downgraded: bool,
    ) -> Result<Vec<(NoteId, f64, String)>, GradatumError> {
        Ok(vec![])
    }
    async fn upsert_override_raw(
        &self,
        _note_id: NoteId,
        _scope: &OverrideScope,
        _override_type: &str,
        _schema_version: u32,
        _payload_toml: &str,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn get_override_raw(
        &self,
        _note_id: NoteId,
        _scope: &OverrideScope,
        _override_type: &str,
    ) -> Result<Option<(u32, String)>, GradatumError> {
        Ok(None)
    }
    async fn upsert_file_checksum(&self, _entry: &FileChecksumEntry) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn list_file_checksums(&self) -> Result<Vec<FileChecksumEntry>, GradatumError> {
        Ok(vec![])
    }
    async fn get_note_created_and_indegree(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<(i64, u64), GradatumError> {
        Ok((0, 0))
    }
    async fn search_fts_with_snippet(
        &self,
        _vault_id: &AclCheckedVaultId,
        _query: &str,
        _limit: usize,
        _include_downgraded: bool,
        _section: Option<&str>,
        _locus: Option<&str>,
        _status: Option<&str>,
        _from_ms: Option<i64>,
        _to_ms: Option<i64>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError> {
        Ok(vec![])
    }
    async fn recall_lessons(
        &self,
        _vault_id: &VaultId,
        _class: &str,
        _limit: usize,
    ) -> Result<Vec<LessonHitRaw>, GradatumError> {
        Ok(vec![])
    }
    async fn list_review_queue(
        &self,
        _vault_id: &VaultId,
        _cursor: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<gradatum_core::ReviewQueueRow>, GradatumError> {
        Ok(vec![])
    }
    async fn count_review_queue(&self, _vault_id: &VaultId) -> Result<u64, GradatumError> {
        Ok(0)
    }
    /// Simule une panne DB sur `find_promotable` — cœur du finding P1-b.
    async fn find_promotable(
        &self,
        _cutoff_ms: i64,
        _limit: usize,
    ) -> Result<Vec<(String, NoteStatus)>, GradatumError> {
        Err(GradatumError::Storage(
            "test: connexion DB forcée en échec pour vérifier la remontée d'erreur".into(),
        ))
    }
    async fn title_lookup(
        &self,
        _vault_id: &str,
        _title: &str,
    ) -> Result<Option<String>, GradatumError> {
        Ok(None)
    }
    async fn id_lookup(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        Ok(None)
    }
    async fn live_note_count(&self, _vault_id: &str) -> Result<u64, GradatumError> {
        Ok(0)
    }
    async fn distinct_authors(&self, _vault_id: &str) -> Result<Vec<AuthorRow>, GradatumError> {
        Ok(vec![])
    }
    async fn distinct_tags(&self, _vault_id: &str) -> Result<Vec<(String, u64)>, GradatumError> {
        Ok(vec![])
    }
    async fn neighbors(
        &self,
        _vault_id: &str,
        _note_id: &str,
        _depth: u8,
    ) -> Result<Vec<String>, GradatumError> {
        Ok(vec![])
    }
    async fn backlinks(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<Vec<String>, GradatumError> {
        Ok(vec![])
    }
    async fn trace_lineage(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<Lineage, GradatumError> {
        Ok(Lineage::default())
    }
    async fn list_notes(
        &self,
        _vault_id: &str,
        _section: Option<&str>,
        _limit: usize,
        _cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        Ok((vec![], 0))
    }
    async fn list_recent_notes(
        &self,
        _vault_id: &str,
        _k: usize,
    ) -> Result<Vec<NoteRecord>, GradatumError> {
        Ok(vec![])
    }
    async fn total_body_size_bytes(&self, _vault_id: &str) -> Result<u64, GradatumError> {
        Ok(0)
    }
    async fn last_indexed_at(&self, _vault_id: &str) -> Result<Option<i64>, GradatumError> {
        Ok(None)
    }
    async fn upsert_link(
        &self,
        _vault_id: &str,
        _src_note_id: &str,
        _dst_note_id: &str,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn get_titles_sections(
        &self,
        _vault_id: &AclCheckedVaultId,
        _ids: &[String],
    ) -> Result<HashMap<String, (Option<String>, String)>, GradatumError> {
        Ok(HashMap::new())
    }
    async fn get_trust(&self, _vault_id: &str, _id: &NoteId) -> Result<Option<f32>, GradatumError> {
        Ok(None)
    }
    async fn upsert_redirect(
        &self,
        _vault_id: &str,
        _slug: &str,
        _ulid: &Ulid,
        _renamed_at_ms: i64,
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn resolve_redirect(
        &self,
        _vault_id: &str,
        _slug: &str,
    ) -> Result<Option<Ulid>, GradatumError> {
        Ok(None)
    }
    async fn search_fts_for_forget(
        &self,
        _vault_id: &VaultId,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        Ok(vec![])
    }
    async fn list_notes_by_locus_prefix(
        &self,
        _vault_id: &str,
        _prefix: &str,
    ) -> Result<Vec<(String, String)>, GradatumError> {
        Ok(vec![])
    }
    async fn list_notes_by_agent(
        &self,
        _agent_id: &str,
        _vaults: &[String],
    ) -> Result<Vec<(String, String)>, GradatumError> {
        Ok(vec![])
    }
    async fn timeline(
        &self,
        _vault_id: &AclCheckedVaultId,
        _filter: &gradatum_core::temporal_query::TimelineFilter,
    ) -> Result<Vec<gradatum_core::temporal_query::TimelineRow>, GradatumError> {
        Ok(vec![])
    }
}

#[async_trait]
impl VectorStore for FailFindPromotableIndex {
    async fn insert_note_embedding(
        &self,
        _vault_id: &str,
        _note_id: &NoteId,
        _embedder_id: &str,
        _dim: u16,
        _vector: &[f32],
    ) -> Result<(), GradatumError> {
        Ok(())
    }
    async fn get_note_embedding(
        &self,
        _vault_id: &str,
        _note_id: &NoteId,
        _embedder_id: &str,
    ) -> Result<Option<Vec<f32>>, GradatumError> {
        Ok(None)
    }
    async fn search_semantic(
        &self,
        _vault_id: &AclCheckedVaultId,
        _embedder_id: &str,
        _query_emb: &[f32],
        _limit: usize,
        _locus: Option<&str>,
    ) -> Result<Vec<(NoteId, f32)>, GradatumError> {
        Ok(vec![])
    }
}

/// Construit un vault réel (TempDir) et retourne `(vault, _dir)`.
async fn build_vault() -> (Arc<Vault>, TempDir) {
    let dir = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    (vault, dir)
}

/// Construit un `Frontmatter` minimal valide pour les tests.
fn minimal_frontmatter() -> Frontmatter {
    use chrono::Utc;
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Draft,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Insère une note via le vault (Draft → `target_status`) et retourne (ULID string, NoteId).
///
/// Utilise `vault.update_status` pour amener la note au statut cible via la state machine.
/// La note est créée au moment réel → pour la simuler « âgée », appeler `promote_once`
/// avec `now_ms = real_now + 20_days_ms`.
async fn seed_vault_note_at_status(vault: &Vault, target_status: NoteStatus) -> (String, NoteId) {
    let fm = minimal_frontmatter();
    let note_id = NoteId::new();
    let note_id_str = note_id.0.to_string();

    vault
        .write_note_with_id(fm, format!("# test {note_id_str}\n\ncorps"), note_id)
        .await
        .expect("write_note_with_id");

    // Amener la note au statut cible via les transitions légales.
    match target_status {
        NoteStatus::Draft => {}
        NoteStatus::PendingReview => {
            vault
                .update_status(note_id, NoteStatus::PendingReview, None)
                .await
                .expect("Draft→PendingReview");
        }
        NoteStatus::Staging => {
            vault
                .update_status(note_id, NoteStatus::PendingReview, None)
                .await
                .expect("Draft→PendingReview");
            vault
                .update_status(note_id, NoteStatus::Staging, None)
                .await
                .expect("PendingReview→Staging");
        }
        other => panic!("target_status {other:?} non supporté dans seed_vault_note_at_status"),
    }

    (note_id_str, note_id)
}

/// `now_ms` futur (20 jours dans le futur depuis le moment réel).
///
/// Les notes créées « maintenant » ont `COALESCE(status_changed, created) = real_now`.
/// Avec `now_ms = real_now + 20j`, le cutoff 14j donne `cutoff = real_now + 6j > real_now`
/// → les notes semblent âgées de 20j ≥ 14j. Elles sont donc éligibles.
fn now_plus_20_days() -> i64 {
    let real_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    real_now + 20 * 86_400_000
}

/// `now_ms` récent (5 jours dans le futur depuis le moment réel).
///
/// Avec `now_ms = real_now + 5j`, le cutoff 14j donne `cutoff = real_now - 9j < real_now`
/// → les notes récentes (status_changed ≈ real_now) ne sont PAS éligibles.
fn now_plus_5_days() -> i64 {
    let real_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    real_now + 5 * 86_400_000
}

/// Config par défaut pour les tests — age_days=14.
fn default_cfg() -> ReviewPromoteConfig {
    ReviewPromoteConfig {
        enabled: true,
        age_days: 14,
        interval_secs: 3600,
        max_per_tick: 200,
    }
}

/// Emballe une `ReviewPromoteConfig` dans une `ServerConfig` par défaut (L6 : `promote_tick`
/// prend désormais la config complète pour résoudre `review_promote_for` par vault ; `per_vault`
/// vide par défaut ⇒ tout vault retombe sur cette config globale).
fn server_cfg(rp: ReviewPromoteConfig) -> ServerConfig {
    ServerConfig {
        review_promote: rp,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Test 1 : staging âgée → promue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promote_once_promotes_aged_staging() {
    let (vault, _dir) = build_vault().await;
    let metrics = AppMetrics::new();
    let cfg = default_cfg();

    let (note_id_str, _) = seed_vault_note_at_status(&vault, NoteStatus::Staging).await;

    // now_ms = real_now + 20j → note est « âgée » de 20j, cutoff = 14j → éligible.
    let now_ms = now_plus_20_days();

    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;

    let stats = promote_once(&index_arc, &vault_arc, &metrics, &cfg, now_ms).await;

    assert_eq!(stats.staging, 1, "stats.staging doit être 1");
    assert_eq!(stats.pending_review, 0);
    assert_eq!(stats.errors, 0);

    // Vérifier que la note est maintenant Live dans le vault.
    let note = vault
        .read_note_by_id(&note_id_str)
        .await
        .expect("read_note_by_id");
    assert_eq!(
        note.frontmatter.status,
        NoteStatus::Live,
        "la note doit être Live après promotion"
    );

    // Métrique incrémentée.
    let counter_val = metrics
        .review_promoted
        .get_or_create(&gradatum_server::metrics::FromStatusLabel {
            from_status: "staging",
        })
        .get();
    assert_eq!(
        counter_val, 1,
        "métrique review_promoted{{staging}} doit être 1"
    );
}

// ---------------------------------------------------------------------------
// Test 2 : note récente → non promue
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promote_once_does_not_promote_recent() {
    let (vault, _dir) = build_vault().await;
    let metrics = AppMetrics::new();
    let cfg = default_cfg();

    seed_vault_note_at_status(&vault, NoteStatus::Staging).await;

    // now_ms = real_now + 5j → note est « âgée » de 5j, cutoff = 14j → NON éligible.
    let now_ms = now_plus_5_days();

    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;

    let stats = promote_once(&index_arc, &vault_arc, &metrics, &cfg, now_ms).await;

    assert_eq!(
        stats,
        PromoteStats::default(),
        "aucune note ne doit être promue"
    );
}

// ---------------------------------------------------------------------------
// Test 3 : cfg.enabled=false → aucune promotion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promote_once_disabled_skips_all() {
    let (vault, _dir) = build_vault().await;
    let metrics = AppMetrics::new();
    let mut cfg = default_cfg();
    cfg.enabled = false;

    seed_vault_note_at_status(&vault, NoteStatus::Staging).await;

    let now_ms = now_plus_20_days();

    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;

    let stats = promote_once(&index_arc, &vault_arc, &metrics, &cfg, now_ms).await;

    assert_eq!(stats, PromoteStats::default(), "disabled → stats zeroes");
}

// ---------------------------------------------------------------------------
// Test 4 : pending-review âgée → promue avec label from_status=pending-review
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promote_once_promotes_pending_review() {
    let (vault, _dir) = build_vault().await;
    let metrics = AppMetrics::new();
    let cfg = default_cfg();

    let (note_id_str, _) = seed_vault_note_at_status(&vault, NoteStatus::PendingReview).await;

    let now_ms = now_plus_20_days();

    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;

    let stats = promote_once(&index_arc, &vault_arc, &metrics, &cfg, now_ms).await;

    assert_eq!(stats.pending_review, 1, "stats.pending_review doit être 1");
    assert_eq!(stats.staging, 0);
    assert_eq!(stats.errors, 0);

    // Note promue en Live dans le vault.
    let note = vault
        .read_note_by_id(&note_id_str)
        .await
        .expect("read_note_by_id");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);

    // Métrique avec label from_status="pending-review".
    let counter_val = metrics
        .review_promoted
        .get_or_create(&gradatum_server::metrics::FromStatusLabel {
            from_status: "pending-review",
        })
        .get();
    assert_eq!(
        counter_val, 1,
        "métrique review_promoted{{pending-review}} doit être 1"
    );
}

// ---------------------------------------------------------------------------
// Test 5 (P1-b) : find_promotable échoue → stats.errors=1 (outcome=error reporté)
// ---------------------------------------------------------------------------

/// Échec de `find_promotable` (panne DB) → `stats.errors = 1`.
///
/// Avant le fix, `promote_once` retournait `PromoteStats::default()` (errors=0)
/// sur échec de `find_promotable`, ce qui faisait que `main.rs` rapportait
/// `TaskOutcome::Ok` malgré la panne — le monitoring était aveugle.
///
/// Après le fix : `stats.errors = 1` → `main.rs` mappe en `TaskOutcome::Error`.
#[tokio::test]
async fn promote_once_db_error_reflected_in_stats() {
    let metrics = AppMetrics::new();
    let cfg = default_cfg();

    // Index qui échoue toujours sur find_promotable.
    let index_arc: Arc<dyn Index> = Arc::new(FailFindPromotableIndex);
    // Le vault n'est jamais appelé si find_promotable échoue en premier.
    let (vault, _dir) = build_vault().await;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;

    let now_ms = now_plus_20_days();
    let stats = promote_once(&index_arc, &vault_arc, &metrics, &cfg, now_ms).await;

    assert_eq!(
        stats.errors, 1,
        "échec find_promotable doit incrémenter stats.errors à 1 (était 0 avant fix)"
    );
    assert_eq!(
        stats.staging, 0,
        "aucune promotion possible si find_promotable échoue"
    );
    assert_eq!(stats.pending_review, 0);
}

// ---------------------------------------------------------------------------
// Tests C2 (EX-C2-3) — promote_tick : OFF = legacy, ON = per-vault
// ---------------------------------------------------------------------------

/// OFF : `promote_tick` délègue strictement à `promote_once` — une note âgée est
/// promue exactement comme avant (plan de tick identique, A6).
#[tokio::test]
async fn promote_tick_off_delegates_to_legacy_scan() {
    let (vault, _dir) = build_vault().await;
    let metrics = AppMetrics::new();
    let cfg = default_cfg();
    let (note_id_str, _) = seed_vault_note_at_status(&vault, NoteStatus::Staging).await;
    let now_ms = now_plus_20_days();

    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;
    // OFF : le registre n'est pas consulté (délégation `promote_once`), mais le paramètre
    // est requis — singleton `main` cohérent avec le handle legacy.
    let vaults = Arc::new(VaultRegistry::singleton(Arc::clone(&vault)));

    let stats = promote_tick(
        &index_arc,
        &vault_arc,
        &vaults,
        &metrics,
        &server_cfg(cfg),
        now_ms,
        false,
    )
    .await;
    assert_eq!(stats.staging, 1, "OFF : promotion identique au legacy");
    assert_eq!(stats.errors, 0);
    let note = vault
        .read_note_by_id(&note_id_str)
        .await
        .expect("read_note_by_id");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);
}

/// OFF : le chemin legacy n'appelle JAMAIS `list_active_vaults` — prouvé avec un
/// index dont `find_promotable` échoue (le tick échoue via le scan global, pas via
/// le listing per-vault).
#[tokio::test]
async fn promote_tick_off_uses_global_scan_error_path() {
    let metrics = AppMetrics::new();
    let cfg = default_cfg();
    let index_arc: Arc<dyn Index> = Arc::new(FailFindPromotableIndex);
    let (vault, _dir) = build_vault().await;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;
    let vaults = Arc::new(VaultRegistry::singleton(Arc::clone(&vault)));

    let stats = promote_tick(
        &index_arc,
        &vault_arc,
        &vaults,
        &metrics,
        &server_cfg(cfg),
        now_plus_20_days(),
        false,
    )
    .await;
    assert_eq!(
        stats.errors, 1,
        "OFF : échec du scan GLOBAL (find_promotable)"
    );
}

/// ON : itération PAR vault actif — la note âgée du vault "main" (tenant actif du
/// seed 0030) est promue via `find_promotable_in_vault` ; une note âgée insérée
/// dans un vault SANS tenant actif n'est PAS touchée (INV-JOB-SCOPE : le tick ne
/// traite que les vaults actifs, jamais un scan cross-vault).
#[tokio::test]
async fn promote_tick_on_iterates_active_vaults_only() {
    let (vault, _dir) = build_vault().await;
    let metrics = AppMetrics::new();
    let cfg = default_cfg();
    let (note_id_str, _) = seed_vault_note_at_status(&vault, NoteStatus::Staging).await;

    // Note âgée dans un vault "orphan" SANS ligne tenants → hors périmètre à ON.
    let orphan_id = "01ZZZZZZZZZZZZZZZZZZZZZZZZ";
    {
        let idx = vault.index();
        idx.seed_note_with_fts_vault(orphan_id, "orphan", "reference", None, "corps orphan")
            .await
            .expect("seed note orphan");
        idx.patch_note_status(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("orphan"),
            ),
            &NoteId(Ulid::from_string(orphan_id).expect("ulid orphan valide — littéral test")),
            Some("staging"),
            None,
            None,
        )
        .await
        .expect("patch staging orphan");
    }

    let now_ms = now_plus_20_days();
    let index_arc = Arc::clone(vault.index()) as Arc<dyn Index>;
    let vault_arc = Arc::clone(&vault) as Arc<dyn Registry>;
    // ON : seul `main` est actif (seed 0030) → le registre doit résoudre `main`. Le vault
    // `orphan` n'a pas de ligne tenants → jamais listé, jamais résolu.
    let vaults = Arc::new(VaultRegistry::singleton(Arc::clone(&vault)));

    let stats = promote_tick(
        &index_arc,
        &vault_arc,
        &vaults,
        &metrics,
        &server_cfg(cfg),
        now_ms,
        true,
    )
    .await;

    assert_eq!(
        stats.staging, 1,
        "ON : la note du vault actif main est promue"
    );
    assert_eq!(
        stats.errors, 0,
        "ON : la note du vault orphan n'est jamais tentée (aucune erreur NoteNotFound)"
    );
    let note = vault
        .read_note_by_id(&note_id_str)
        .await
        .expect("read_note_by_id");
    assert_eq!(note.frontmatter.status, NoteStatus::Live);

    // La note du vault orphan reste staging (jamais touchée) : toujours promotable.
    let cutoff = now_ms - 14 * 86_400_000;
    let still_promotable = vault
        .index()
        .find_promotable_in_vault("orphan", cutoff, 100)
        .await
        .expect("find_promotable_in_vault orphan");
    assert_eq!(
        still_promotable.len(),
        1,
        "vault inactif : note intouchée à ON (toujours staging/promotable)"
    );
}
