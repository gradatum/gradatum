//! Tests F-74 v0.7.4 — Temporal Anchor Population (Tasks 3 + 4).
//!
//! ## Couverture
//!
//! - `curate_with_occurred_at_sets_anchor_src_occurred_at` (rouge→vert) :
//!   `CurateSpec.occurred_at = Some("2026-01-15")` → `persist_curated` reçoit
//!   `temporal.anchor_src = "occurred_at"` et `temporal.anchor_ms = ms(2026-01-15T00:00:00Z)`.
//!
//! - `curate_without_occurred_at_anchor_src_is_created` (backward-compat) :
//!   `CurateSpec.occurred_at = None` → `temporal.anchor_src = "created"`,
//!   `temporal.anchor_ms ≈ now`.
//!
//! ## Harness
//!
//! `handle_curate` directement (worker in-process, sans HTTP).
//! Pattern identique à `curate_embed_chaining.rs` : `TestInternalClient` + `SqliteQueueStore`.
//!
//! ## Architecture note
//!
//! `vault_write` HTTP → job enqueué dans `SqliteQueueStore` (apalis, JSON). Les tests
//! E2E de dispatch appellent `handle_curate` directement (le moteur actif).

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::scope::VaultId;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse, TemporalEntryDto,
};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::handle_curate;
use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteIdDto, NoteReadDto,
};
use tempfile::TempDir;
use tokio::sync::Mutex;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Crée un `SqliteQueueStore` in-memory avec schéma appliqué.
async fn test_store() -> SqliteQueueStore {
    let db = QueueDb::open_in_memory()
        .await
        .expect("pool in-memory — invariant test fixture");
    apply_sqlite_pragmas(&db)
        .await
        .expect("pragmas — invariant test fixture");
    run_migrations(&db)
        .await
        .expect("migrations — invariant test fixture");
    SqliteQueueStore::new(db)
}

/// Fixture partagée : Vault + SqliteIndex dans TempDir.
struct CurateFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    _tmp: TempDir,
}

impl CurateFixture {
    async fn new() -> Self {
        let tmp = TempDir::new().expect("TempDir — invariant test fixture");
        let vault = Arc::new(
            Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
                .await
                .expect("Vault::create — invariant test fixture"),
        );
        let index = vault.index().clone();
        CurateFixture {
            vault,
            index,
            _tmp: tmp,
        }
    }
}

/// Construit un `GradatumJob` curate vault_write (title + body présents).
///
/// `occurred_at` : `Some("2026-01-15")` pour tester F-74, `None` pour backward-compat.
fn make_curate_job_with_occurred_at(
    title: &str,
    body: &str,
    occurred_at: Option<&str>,
) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id: Ulid::generate(),
                    tenant_id: "main".to_string(),
                    title: Some(title.to_string()),
                    body: Some(body.to_string()),
                    occurred_at: occurred_at.map(|s| s.to_string()),
                    ..Default::default()
                }),
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::High,
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Running,
                created_at: now,
                started_at: Some(now),
                completed_at: None,
                lease_until: None,
                result: None,
            },
            retry: JobRetry::default(),
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CapturingClient — capture temporal depuis persist_curated
// ─────────────────────────────────────────────────────────────────────────────

/// Valeurs temporelles capturées depuis `PersistCuratedRequest.temporal`.
///
/// `TemporalEntryDto` ne dérive pas `Clone` — on extrait seulement les champs
/// nécessaires aux assertions (`anchor_src`, `anchor_ms`).
#[derive(Debug, Clone)]
struct CapturedTemporal {
    anchor_src: String,
    anchor_ms: i64,
}

/// Client de test qui capture le temporal reçu dans `persist_curated`.
///
/// Écrit la note dans le vault pour que handle_curate fonctionne normalement.
/// Le temporal capturé est accessible via `captured_temporal` après le dispatch.
struct CapturingClient {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    captured_temporal: Arc<Mutex<Option<CapturedTemporal>>>,
}

#[async_trait]
impl InternalClient for CapturingClient {
    async fn persist_curated(
        &self,
        req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        // Capturer les champs temporels AVANT toute opération vault.
        // On n'extrait que anchor_src + anchor_ms (champs des assertions).
        if let Some(ref temporal) = req.temporal {
            *self.captured_temporal.lock().await = Some(CapturedTemporal {
                anchor_src: temporal.anchor_src.clone(),
                anchor_ms: temporal.anchor_ms,
            });
        }

        // Déléguer l'écriture vault à TestInternalClient réutilisé (IMPORT > COPIER).
        let inner = test_internal_client::TestInternalClient::new(
            Arc::clone(&self.vault),
            Arc::clone(&self.index),
        );
        inner.persist_curated(req).await
    }

    async fn persist_embedding(
        &self,
        _req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError> {
        Ok(EmbeddingOkResponse {
            note_id: String::new(),
            embedder_id: "noop".to_string(),
            dim: 0,
        })
    }

    async fn persist_forget(
        &self,
        _req: &PersistForgetRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        Ok(PersistOkResponse {
            note_id: String::new(),
            status: "ok".to_string(),
        })
    }

    async fn persist_distill(
        &self,
        _req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        Ok(PersistOkResponse {
            note_id: String::new(),
            status: "ok".to_string(),
        })
    }

    async fn delete_note(&self, _vault_id: &str, _ulid: &str) -> Result<(), InternalClientError> {
        Ok(())
    }

    async fn get_note(
        &self,
        vault_id: &str,
        ulid: &str,
    ) -> Result<NoteReadDto, InternalClientError> {
        // Délègue au TestInternalClient qui lit depuis le vault (chemins reclassification C-1).
        test_internal_client::TestInternalClient::new(
            Arc::clone(&self.vault),
            Arc::clone(&self.index),
        )
        .get_note(vault_id, ulid)
        .await
    }

    async fn get_note_status(
        &self,
        vault_id: &str,
        ulid: &str,
    ) -> Result<Option<String>, InternalClientError> {
        test_internal_client::TestInternalClient::new(
            Arc::clone(&self.vault),
            Arc::clone(&self.index),
        )
        .get_note_status(vault_id, ulid)
        .await
    }

    async fn get_note_embedding(
        &self,
        _vault_id: &str,
        _ulid: &str,
        _embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        Err(InternalClientError::NotFound {
            ulid: _ulid.to_string(),
        })
    }

    async fn get_trust(&self, _vault_id: &str, _ulid: &str) -> Result<f32, InternalClientError> {
        Ok(0.5)
    }

    async fn title_lookup(
        &self,
        _tenant: &str,
        _title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        Ok(None)
    }

    async fn id_lookup(
        &self,
        _tenant: &str,
        _note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        Ok(None)
    }

    async fn list_notes_by_locus(
        &self,
        _vault: &str,
        _prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }

    async fn list_by_status(
        &self,
        _vault: &str,
        _status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }

    async fn list_garbage(
        &self,
        _vault: &str,
        _before_ms: i64,
        _grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }

    async fn search_fts_for_forget(
        &self,
        _vault: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }

    async fn list_notes_by_agent(
        &self,
        _agent: &str,
        _vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// occurred_at:"2026-01-15" → anchor_src="occurred_at" + anchor_ms correct.
///
/// Valide que la chaîne `CurateSpec.occurred_at` → ExtraFields → `resolve_temporal_anchor`
/// → `PersistCuratedRequest.temporal` produit l'ancre événementielle attendue.
///
/// `anchor_ms` attendu = ms(2026-01-15T00:00:00Z) — début de journée UTC (format YYYY-MM-DD).
#[tokio::test]
async fn curate_with_occurred_at_sets_anchor_src_occurred_at() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    // Préfixe [DECISIONS] → heuristique Admitted direct.
    let job = make_curate_job_with_occurred_at(
        "[DECISIONS] Test temporal anchor F-74 occurred_at",
        "# Test\n\nContenu suffisant pour être admis (>20 chars) — test temporal anchor.",
        Some("2026-01-15"),
    );

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok — err={result:?}"
    );

    // Vérifier le temporal capturé par persist_curated.
    let temporal = captured_temporal
        .lock()
        .await
        .clone()
        .expect("persist_curated doit recevoir un TemporalEntryDto (note admise)");

    // anchor_src doit être "occurred_at" (F-74 activé)
    assert_eq!(
        temporal.anchor_src, "occurred_at",
        "anchor_src doit être 'occurred_at' quand spec.occurred_at est Some('2026-01-15')"
    );

    // anchor_ms doit correspondre à 2026-01-15T00:00:00Z (début de journée UTC)
    let expected_ms = chrono::DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
        .expect("parsing date de référence — invariant test")
        .timestamp_millis();
    assert_eq!(
        temporal.anchor_ms, expected_ms,
        "anchor_ms doit correspondre à 2026-01-15T00:00:00Z (ms={expected_ms})"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers C-1 (branche A — vault_write RMW)
// ─────────────────────────────────────────────────────────────────────────────

/// Convertit un sha256_hex (64 chars) en `[u8; 32]`.
///
/// `.expect()` autorisé : invariant de test (sha256_hex provient du vault, format garanti).
fn hex_to_bytes32(hex: &str) -> [u8; 32] {
    assert_eq!(
        hex.len(),
        64,
        "sha256_hex doit faire 64 chars — invariant fixture"
    );
    let mut arr = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).expect("hex ascii — invariant");
        arr[i] = u8::from_str_radix(s, 16).expect("hex digit valide — invariant");
    }
    arr
}

/// Construit un job vault_write **RMW** (title+body présents, sha256 fourni).
///
/// title+body présents → `note_id_for_vault = None` (branche A dans handle_curate).
/// `expected_sha256 = Some(_)` → discriminateur RMW update (vs CREATE sha=None).
fn make_vault_write_rmw_job(
    note_id: Ulid,
    title: &str,
    body: &str,
    expected_sha256: [u8; 32],
    occurred_at: Option<&str>,
) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id,
                    tenant_id: "main".to_string(),
                    title: Some(title.to_string()),
                    body: Some(body.to_string()),
                    expected_sha256: Some(expected_sha256),
                    occurred_at: occurred_at.map(|s| s.to_string()),
                    ..Default::default()
                }),
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::High,
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Running,
                created_at: now,
                started_at: Some(now),
                completed_at: None,
                lease_until: None,
                result: None,
            },
            retry: JobRetry::default(),
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers C-1 (branche B — reclassification)
// ─────────────────────────────────────────────────────────────────────────────

/// Sème une note dans vault + temporal_index avec l'ancre fournie.
///
/// Utilisé pour préparer les tests C-1 (reclassification/RMW) : la note doit
/// exister dans le vault pour que `client.get_note()` la retrouve.
async fn seed_note_with_anchor(
    vault: &Arc<Vault>,
    index: &Arc<SqliteIndex>,
    note_id: Ulid,
    body: &str,
    anchor_src: &str,
    anchor_ms: i64,
) {
    let inner = test_internal_client::TestInternalClient::new(Arc::clone(vault), Arc::clone(index));
    let mut req = PersistCuratedRequest::new(
        note_id.to_string(),
        "main".to_string().into(),
        "C-1 seed note".to_string(),
        body.to_string(),
        "decisions".to_string(),
        "live".to_string(),
    );
    req.temporal = Some(TemporalEntryDto {
        anchor_ms,
        anchor_src: anchor_src.to_string(),
        doc_kind: "note".to_string(),
        valid_until_ms: None,
    });
    inner
        .persist_curated(&req)
        .await
        .expect("seed_note_with_anchor — invariant test fixture");
}

/// Construit un `GradatumJob` Curate en mode **reclassification** (title + body absents).
///
/// title=None + body=None → `note_id_for_vault = Some(_)` dans `handle_curate` —
/// c'est la branche buggée corrigée par C-1.
fn make_reclassify_job(note_id: Ulid, occurred_at: Option<&str>) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id,
                    tenant_id: "main".to_string(),
                    title: None,
                    body: None,
                    occurred_at: occurred_at.map(|s| s.to_string()),
                    ..Default::default()
                }),
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::High,
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Running,
                created_at: now,
                started_at: Some(now),
                completed_at: None,
                lease_until: None,
                result: None,
            },
            retry: JobRetry::default(),
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests C-1 — TDD rouge d'abord (ces tests DOIVENT échouer avant le correctif)
// ─────────────────────────────────────────────────────────────────────────────

/// C-1 Test 1 — reclassify avec occurred_at → anchor_src="occurred_at", jamais now().
///
/// Valide que la branche `note_id_for_vault = Some` honore `spec.occurred_at`
/// (symétrie avec le chemin CREATE). Avant C-1, cette branche hardcodait
/// `Utc::now() + AnchorSrc::Created`.
#[tokio::test]
async fn reclassify_with_occurred_at_uses_occurred_at_anchor() {
    let fixture = CurateFixture::new().await;
    let note_id = Ulid::generate();

    // Seed : note existante dans le vault avec une ancre Created historique.
    let old_created_ms = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
        .expect("parsing date — invariant test")
        .timestamp_millis();
    seed_note_with_anchor(
        &fixture.vault,
        &fixture.index,
        note_id,
        "# [DECISIONS] C-1 test — reclassify avec occurred_at\n\nContenu de test correctif C-1 (>20 chars).",
        "created",
        old_created_ms,
    )
    .await;

    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    // Reclassification avec occurred_at=2026-06-29
    let job = make_reclassify_job(note_id, Some("2026-06-29"));

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok — err={result:?}"
    );

    let temporal = captured_temporal
        .lock()
        .await
        .clone()
        .expect("persist_curated doit recevoir TemporalEntryDto quand occurred_at est fourni");

    assert_eq!(
        temporal.anchor_src, "occurred_at",
        "C-1 : reclassify avec occurred_at doit ancrer sur 'occurred_at', pas 'created'"
    );

    let expected_ms = chrono::DateTime::parse_from_rfc3339("2026-06-29T00:00:00Z")
        .expect("parsing date — invariant test")
        .timestamp_millis();
    assert_eq!(
        temporal.anchor_ms, expected_ms,
        "C-1 : anchor_ms doit valoir 2026-06-29T00:00:00Z (ms={expected_ms}), jamais now()"
    );
}

/// C-1 Test 2 — reclassify sans occurred_at → ancre existante préservée (zéro clobber).
///
/// Note ancrée sur occurred_at (2026-01-15). Reclassification sans occurred_at dans
/// le spec. Avant C-1, la branche `note_id_for_vault = Some` écrasait l'ancre par
/// `Utc::now() + Created`. Après C-1, court-circuit : temporal=None → INSERT OR
/// REPLACE non exécuté → ancre inchangée dans temporal_index.
#[tokio::test]
async fn reclassify_without_occurred_at_no_clobber_existing_anchor() {
    let fixture = CurateFixture::new().await;
    let note_id = Ulid::generate();

    let original_anchor_ms = chrono::DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
        .expect("parsing date — invariant test")
        .timestamp_millis();

    seed_note_with_anchor(
        &fixture.vault,
        &fixture.index,
        note_id,
        "# [DECISIONS] C-1 test — préservation ancre existante\n\nContenu pour test de non-clobber (>20 chars).",
        "occurred_at",
        original_anchor_ms,
    )
    .await;

    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    // Reclassification SANS occurred_at → court-circuit attendu
    let job = make_reclassify_job(note_id, None);

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok — err={result:?}"
    );

    // Court-circuit : persist_curated doit recevoir temporal=None (pas d'écriture)
    assert!(
        captured_temporal.lock().await.is_none(),
        "C-1 : reclassify sans occurred_at → temporal: None (court-circuit, zéro clobber)"
    );

    // L'entrée temporal_index préexistante doit être INCHANGÉE
    let anchor_map = fixture
        .index
        .get_anchor_ms_batch("main", &[note_id.to_string()])
        .await
        .expect("get_anchor_ms_batch — invariant test");

    let preserved_ms = anchor_map.get(&note_id.to_string()).copied().expect(
        "l'entrée temporal_index doit encore exister après reclassification sans occurred_at",
    );

    assert_eq!(
        preserved_ms, original_anchor_ms,
        "C-1 : anchor_ms préexistant={original_anchor_ms} doit être préservé, pas clobberé par now()"
    );
}

/// C-1 Test 3 — vault_classify d'une note occurred_at → ancre préservée.
///
/// Variante explicite du chemin vault_classify (note existante, spec sans title/body
/// ni occurred_at). Équivalent fonctionnel du Test 2 depuis l'angle vault_classify.
#[tokio::test]
async fn vault_classify_without_occurred_at_preserves_anchor() {
    let fixture = CurateFixture::new().await;
    let note_id = Ulid::generate();

    let original_anchor_ms = chrono::DateTime::parse_from_rfc3339("2026-03-10T00:00:00Z")
        .expect("parsing date — invariant test")
        .timestamp_millis();

    seed_note_with_anchor(
        &fixture.vault,
        &fixture.index,
        note_id,
        "# [DECISIONS] C-1 test — vault_classify ancre inchangée\n\nNote classifiée avec occurred_at (>20 chars).",
        "occurred_at",
        original_anchor_ms,
    )
    .await;

    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    // vault_classify : spec sans title/body ni occurred_at
    let job = make_reclassify_job(note_id, None);

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok — err={result:?}"
    );

    assert!(
        captured_temporal.lock().await.is_none(),
        "C-1 : vault_classify sans occurred_at → temporal: None (zéro clobber)"
    );

    let anchor_map = fixture
        .index
        .get_anchor_ms_batch("main", &[note_id.to_string()])
        .await
        .expect("get_anchor_ms_batch — invariant test");

    let preserved_ms = anchor_map
        .get(&note_id.to_string())
        .copied()
        .expect("temporal_index doit encore exister après vault_classify sans occurred_at");

    assert_eq!(
        preserved_ms, original_anchor_ms,
        "C-1 : vault_classify ne doit pas modifier anchor_ms={original_anchor_ms}"
    );
}

/// C-1 Test 4 — non-régression : note Created-only reclassifiée → ancre Created préservée.
///
/// Note avec anchor_src=Created, reclassifiée sans occurred_at. Avant C-1, la branche
/// écrasait l'ancre par `Utc::now()` (valeur légèrement différente de l'ancre originale).
/// Après C-1, court-circuit : temporal=None → Created entry préservée telle quelle.
#[tokio::test]
async fn reclassify_created_only_no_clobber() {
    let fixture = CurateFixture::new().await;
    let note_id = Ulid::generate();

    // Ancre Created à une date passée précise (pas now()).
    let original_created_ms = chrono::DateTime::parse_from_rfc3339("2024-12-01T10:30:00Z")
        .expect("parsing date — invariant test")
        .timestamp_millis();

    seed_note_with_anchor(
        &fixture.vault,
        &fixture.index,
        note_id,
        "# [DECISIONS] C-1 test — non-régression Created\n\nNote avec ancre Created uniquement (>20 chars).",
        "created",
        original_created_ms,
    )
    .await;

    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    // Reclassification sans occurred_at d'une note purement Created
    let job = make_reclassify_job(note_id, None);

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok — err={result:?}"
    );

    // Court-circuit attendu : pas de ré-écriture de l'ancre Created
    assert!(
        captured_temporal.lock().await.is_none(),
        "C-1 : reclassify d'une note Created sans occurred_at → temporal: None (non-régression)"
    );

    let anchor_map = fixture
        .index
        .get_anchor_ms_batch("main", &[note_id.to_string()])
        .await
        .expect("get_anchor_ms_batch — invariant test");

    let preserved_ms = anchor_map
        .get(&note_id.to_string())
        .copied()
        .expect("l'entrée Created dans temporal_index doit être préservée");

    assert_eq!(
        preserved_ms, original_created_ms,
        "C-1 : anchor_ms Created={original_created_ms} ne doit pas être clobberé par now()"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests C-1 branche A — vault_write RMW (TDD rouge d'abord, REV.2)
// ─────────────────────────────────────────────────────────────────────────────
//
// Branche A = `note_id_for_vault = None` = tout vault_write (CREATE + RMW update).
// Discriminateur CREATE vs RMW : `spec.expected_sha256` (None = CREATE, Some = RMW).
// Les tests 3/4/5 ci-dessous couvrent les cas de la branche A non patchés en v1.

/// C-1 Test 3 — vault_write RMW AVEC occurred_at → anchor_src="occurred_at" (cas F-43).
///
/// Valide que la branche A honore `spec.occurred_at` sur un RMW. Avant C-1 v2 :
/// `resolve_temporal_anchor` avec `occurred_at` → correct, mais tombait après la branche
/// générique. Ce test sert de non-régression pour le chemin occurred_at.
#[tokio::test]
async fn vault_write_rmw_with_occurred_at_sets_anchor_occurred_at() {
    let fixture = CurateFixture::new().await;
    let note_id = Ulid::generate();

    // Seed la note (ancre Created initiale quelconque, on va l'écraser via occurred_at)
    let seed_body =
        "# [DECISIONS] C-1 test — RMW avec occurred_at\n\nContenu seed initial (>20 chars).";
    seed_note_with_anchor(
        &fixture.vault,
        &fixture.index,
        note_id,
        seed_body,
        "created",
        chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .expect("parsing date — invariant test")
            .timestamp_millis(),
    )
    .await;

    // Lire le sha256 de la note seedée pour le lock optimiste
    let sha_bytes = {
        let inner = test_internal_client::TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        );
        let dto = inner
            .get_note("main", &note_id.to_string())
            .await
            .expect("note seedée doit être lisible — invariant fixture");
        hex_to_bytes32(&dto.sha256_hex)
    };

    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    // vault_write RMW avec occurred_at : doit ancrer sur occurred_at, jamais now()
    let rmw_body =
        "# [DECISIONS] C-1 test — RMW avec occurred_at (MAJ)\n\nContenu mis à jour (>20 chars).";
    let job = make_vault_write_rmw_job(
        note_id,
        "[DECISIONS] C-1 test — RMW avec occurred_at (MAJ)",
        rmw_body,
        sha_bytes,
        Some("2026-06-29"),
    );

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate RMW doit retourner Ok — err={result:?}"
    );

    let temporal = captured_temporal
        .lock()
        .await
        .clone()
        .expect("persist_curated doit recevoir TemporalEntryDto quand occurred_at est fourni");

    assert_eq!(
        temporal.anchor_src, "occurred_at",
        "C-1 branche A : RMW avec occurred_at doit ancrer sur 'occurred_at', pas 'created'"
    );

    let expected_ms = chrono::DateTime::parse_from_rfc3339("2026-06-29T00:00:00Z")
        .expect("parsing date — invariant test")
        .timestamp_millis();
    assert_eq!(
        temporal.anchor_ms, expected_ms,
        "C-1 branche A : anchor_ms doit valoir 2026-06-29T00:00:00Z (ms={expected_ms})"
    );
}

/// C-1 Test 4 — vault_write RMW SANS occurred_at, note ancrée → ancre préservée.
///
/// **LE test qui manquait en v1** (reviewer l'a exécuté manuellement, reproductible).
/// Note ancrée `occurred_at` (2026-01-15). RMW vault_write sans `occurred_at`.
/// Avant C-1 v2 : branche A retourne toujours `Some(resolve_temporal_anchor(...))` →
/// `(now(), Created)` → ancre clobberée. Après C-1 v2 : discriminateur sha → court-circuit
/// `temporal: None` → INSERT OR REPLACE non déclenché → ancre préservée.
#[tokio::test]
async fn vault_write_rmw_without_occurred_at_preserves_existing_anchor() {
    let fixture = CurateFixture::new().await;
    let note_id = Ulid::generate();

    let original_anchor_ms = chrono::DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
        .expect("parsing date — invariant test")
        .timestamp_millis();

    let seed_body = "# [DECISIONS] C-1 test — RMW sans occurred_at (ancre préservée)\n\nContenu seed (>20 chars).";
    seed_note_with_anchor(
        &fixture.vault,
        &fixture.index,
        note_id,
        seed_body,
        "occurred_at",
        original_anchor_ms,
    )
    .await;

    let sha_bytes = {
        let inner = test_internal_client::TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        );
        let dto = inner
            .get_note("main", &note_id.to_string())
            .await
            .expect("note seedée doit être lisible — invariant fixture");
        hex_to_bytes32(&dto.sha256_hex)
    };

    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    // vault_write RMW SANS occurred_at — l'ancre existante doit être préservée
    let rmw_body = "# [DECISIONS] C-1 test — RMW sans occurred_at (ancre préservée — MAJ)\n\nMise à jour sans occurred_at (>20 chars).";
    let job = make_vault_write_rmw_job(
        note_id,
        "[DECISIONS] C-1 test — RMW sans occurred_at (MAJ)",
        rmw_body,
        sha_bytes,
        None, // pas de occurred_at → court-circuit attendu
    );

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate RMW doit retourner Ok — err={result:?}"
    );

    // Court-circuit : temporal: None → persist_curated ne reçoit PAS de TemporalEntryDto
    assert!(
        captured_temporal.lock().await.is_none(),
        "C-1 branche A : RMW sans occurred_at → temporal: None (court-circuit, zéro clobber)"
    );

    // L'entrée temporal_index préexistante doit être INCHANGÉE
    let anchor_map = fixture
        .index
        .get_anchor_ms_batch("main", &[note_id.to_string()])
        .await
        .expect("get_anchor_ms_batch — invariant test");

    let preserved_ms = anchor_map
        .get(&note_id.to_string())
        .copied()
        .expect("temporal_index doit encore exister après RMW sans occurred_at");

    assert_eq!(
        preserved_ms, original_anchor_ms,
        "C-1 branche A : anchor_ms={original_anchor_ms} doit être préservé, pas clobberé par now()"
    );
}

/// C-1 Test 5 — vault_write RMW SANS occurred_at, note sans entrée → aucune entrée (now,Created).
///
/// Note sans temporal_index entry. RMW vault_write sans occurred_at.
/// Avant C-1 v2 : branche A écrirait `(now(), Created)` → temporal entry créée spurieusement.
/// Après C-1 v2 : discriminateur sha → court-circuit `temporal: None` → note reste sans entrée.
#[tokio::test]
async fn vault_write_rmw_without_occurred_at_no_spurious_created_entry() {
    let fixture = CurateFixture::new().await;
    let note_id = Ulid::generate();

    // Seed la note SANS entrée temporal_index
    let seed_body = "# [DECISIONS] C-1 test — RMW sans occurred_at (pas d'entrée)\n\nNote sans ancre initiale (>20 chars).";
    let inner_seed = test_internal_client::TestInternalClient::new(
        Arc::clone(&fixture.vault),
        Arc::clone(&fixture.index),
    );
    // temporal reste None (défaut du constructeur) : pas d'entrée initiale.
    let seed_req = PersistCuratedRequest::new(
        note_id.to_string(),
        "main".to_string().into(),
        "C-1 seed note (pas de temporal)".to_string(),
        seed_body.to_string(),
        "decisions".to_string(),
        "live".to_string(),
    );
    inner_seed
        .persist_curated(&seed_req)
        .await
        .expect("seed sans temporal — invariant fixture");

    // Vérifier qu'il n'y a effectivement pas d'entrée initiale
    let pre_map = fixture
        .index
        .get_anchor_ms_batch("main", &[note_id.to_string()])
        .await
        .expect("get_anchor_ms_batch — invariant test");
    assert!(
        !pre_map.contains_key(&note_id.to_string()),
        "précondition : pas d'entrée temporal_index avant le RMW"
    );

    let sha_bytes = {
        let inner = test_internal_client::TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        );
        let dto = inner
            .get_note("main", &note_id.to_string())
            .await
            .expect("note seedée doit être lisible — invariant fixture");
        hex_to_bytes32(&dto.sha256_hex)
    };

    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    let rmw_body = "# [DECISIONS] C-1 test — RMW sans occurred_at (pas d'entrée, MAJ)\n\nMise à jour sans temporal (>20 chars).";
    let job = make_vault_write_rmw_job(
        note_id,
        "[DECISIONS] C-1 test — RMW sans occurred_at (pas d'entrée, MAJ)",
        rmw_body,
        sha_bytes,
        None,
    );

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate RMW doit retourner Ok — err={result:?}"
    );

    // Court-circuit : temporal: None → pas d'écriture
    assert!(
        captured_temporal.lock().await.is_none(),
        "C-1 branche A : RMW sans occurred_at sur note sans entrée → temporal: None (pas de (now,Created) spurieux)"
    );

    // L'entrée temporal_index doit rester ABSENTE après le RMW
    let post_map = fixture
        .index
        .get_anchor_ms_batch("main", &[note_id.to_string()])
        .await
        .expect("get_anchor_ms_batch — invariant test");

    assert!(
        !post_map.contains_key(&note_id.to_string()),
        "C-1 branche A : aucune entrée (now,Created) ne doit être créée lors d'un RMW sans occurred_at"
    );
}

/// backward-compat : sans occurred_at → anchor_src="created" + anchor_ms ≈ now.
///
/// Valide que l'absence de `occurred_at` préserve le comportement historique :
/// `resolve_temporal_anchor` retourne `(created_ms, AnchorSrc::Created)`.
#[tokio::test]
async fn curate_without_occurred_at_anchor_src_is_created() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let captured_temporal: Arc<Mutex<Option<CapturedTemporal>>> = Arc::new(Mutex::new(None));
    let client = Arc::new(CapturingClient {
        vault: Arc::clone(&fixture.vault),
        index: Arc::clone(&fixture.index),
        captured_temporal: Arc::clone(&captured_temporal),
    });

    let before_ms = Utc::now().timestamp_millis();

    // Pas de occurred_at → backward-compat.
    let job = make_curate_job_with_occurred_at(
        "[DECISIONS] Test temporal anchor F-74 backward-compat",
        "# Test\n\nContenu suffisant pour être admis (>20 chars) — backward-compat sans occurred_at.",
        None,
    );

    let result = handle_curate(
        job,
        Data::new(client as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(queue),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    let after_ms = Utc::now().timestamp_millis();

    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok — err={result:?}"
    );

    let temporal = captured_temporal
        .lock()
        .await
        .clone()
        .expect("persist_curated doit recevoir un TemporalEntryDto (note admise)");

    // anchor_src doit être "created" (comportement historique)
    assert_eq!(
        temporal.anchor_src, "created",
        "anchor_src doit être 'created' quand spec.occurred_at est None"
    );

    // anchor_ms doit être dans la fenêtre before_ms..=after_ms + marge 1s
    assert!(
        temporal.anchor_ms >= before_ms && temporal.anchor_ms <= after_ms + 1000,
        "anchor_ms={} doit être dans la fenêtre [{}..={}] (created ≈ now)",
        temporal.anchor_ms,
        before_ms,
        after_ms + 1000
    );
}
