//! Tests TDD F-41 — optimistic-lock dans `handle_curate`.
//!
//! ## Couverture
//!
//! | Test | Comportement validé |
//! |---|---|
//! | `handle_curate_conflict_on_stale_hash` | Job Curate avec `expected_sha256` périmé → `JobStatus::Conflict`, note non écrasée |
//! | `handle_curate_no_expected_sha_writes_normally` | Sans `expected_sha256` → écriture normale (rétrocompat) |
//! | `handle_curate_correct_hash_writes_normally` | Hash correct → écriture réussie |
//!
//! ## Architecture
//!
//! Pattern identique à `curate_prealloc_note_id.rs` :
//! - `CurateFixture` + `SqliteQueueStore` in-memory
//! - Curator `CuratorPipeline::new()` (heuristique locale, pas de LLM)
//! - Préfixe `[DECISIONS]` → confidence ≥ 0.8 → `CurateOutcome::Admitted`
//!
//! ## F-41 — flux testé
//!
//! 1. Écrire note v1 sans expected_sha256 (→ obtenir hash_v1).
//! 2. Écrire note v2 sans expected_sha256 (→ le hash change, hash_v1 devient périmé).
//! 3. Soumettre job Curate avec `expected_sha256 = Some(hash_v1)` → Conflict.
//!    - Note doit rester v2 (non écrasée).
//!    - Job status doit être `JobStatus::Conflict`.
//!    - `lifecycle.result.conflict_payload` doit contenir `current_sha256`.

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::QueueStore;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobResult, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
    identity::{ContentHash, NoteId},
    scope::VaultId,
};
use gradatum_db_sqlite::{SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::handle_curate;
use gradatum_worker::internal_client::InternalClient;
use test_internal_client::TestInternalClient;

use sqlx::SqlitePool;
use tempfile::TempDir;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Crée un `SqliteQueueStore` in-memory avec schéma appliqué.
async fn test_store() -> SqliteQueueStore {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("pool in-memory F-41");
    apply_sqlite_pragmas(&pool).await.expect("pragmas F-41");
    run_migrations(&pool).await.expect("migrations F-41");
    SqliteQueueStore::new(pool)
}

/// Fixture vault + index.
struct CurateFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    _tmp: TempDir,
}

impl CurateFixture {
    async fn new() -> Self {
        let tmp = TempDir::new().expect("TempDir F-41");
        let vault = Arc::new(
            Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
                .await
                .expect("Vault::create F-41"),
        );
        let index = vault.index().clone();
        CurateFixture {
            vault,
            index,
            _tmp: tmp,
        }
    }
}

/// Construit un `GradatumJob` vault_write avec `expected_sha256` optionnel.
///
/// Préfixe `[DECISIONS]` → heuristique confidence ≥ 0.8 → `Admitted`.
fn curate_job_with_hash(
    prealloc: Ulid,
    body: &str,
    expected_sha256: Option<[u8; 32]>,
) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id: prealloc,
                    tenant_id: "main".to_string(),
                    title: Some("[DECISIONS] Test F-41 optimistic-lock".to_string()),
                    body: Some(body.to_string()),
                    expected_sha256,
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
                status: JobStatus::Pending,
                created_at: now,
                started_at: None,
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
// Test 1 — Conflict sur hash périmé
// ─────────────────────────────────────────────────────────────────────────────

/// Job Curate avec `expected_sha256` périmé :
/// - Le job retourne `JobOutput` vide (pas d'erreur handler)
/// - La note n'est PAS écrasée (reste v2)
/// - Le job est marqué `JobStatus::Conflict` dans le store
/// - `lifecycle.result.conflict_payload` contient le `current_sha256`
#[tokio::test]
async fn handle_curate_conflict_on_stale_hash() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn gradatum_core::QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let prealloc = Ulid::new();

    // ── Étape 1 : écrire v1 sans expected_sha256 → obtenir hash_v1 ─────────
    let job_v1 = curate_job_with_hash(prealloc, "# Corps v1\nbody v1.", None);
    let out_v1 = handle_curate(
        job_v1,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate v1 F-41");
    assert!(
        !out_v1.notes_created.is_empty(),
        "v1 doit créer la note — out={out_v1:?}"
    );

    // Lire le hash de v1 depuis le vault.
    let note_v1 = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("read_note v1 F-41");
    let hash_v1: [u8; 32] = note_v1.content_hash.0;

    // ── Étape 2 : écrire v2 sans expected_sha256 → hash_v1 devient périmé ──
    // Construire un nouveau job pour v2 avec le même note_id (reclassification
    // via vault_write, même préalloc → écrasement). Sans expected_sha256 = inconditionnel.
    let job_v2 = curate_job_with_hash(prealloc, "# Corps v2\nbody v2 différent.", None);
    let out_v2 = handle_curate(
        job_v2,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate v2 F-41");
    assert!(
        !out_v2.notes_created.is_empty() || !out_v2.notes_modified.is_empty(),
        "v2 doit écrire la note — out={out_v2:?}"
    );

    // Vérifier que le body de la note est maintenant v2.
    let note_after_v2 = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("read_note après v2 F-41");
    let hash_v2: [u8; 32] = note_after_v2.content_hash.0;
    assert_ne!(hash_v1, hash_v2, "hash v1 et v2 doivent être différents");
    assert!(
        note_after_v2.body.markdown.contains("v2"),
        "note doit contenir v2 après écriture v2"
    );

    // ── Étape 3 : tentative Curate avec hash_v1 périmé ──────────────────────
    let conflict_job = curate_job_with_hash(
        prealloc,
        "# Corps v3 (DOIT être bloqué)\nbody v3.",
        Some(hash_v1), // hash périmé
    );
    // Le handler doit retourner Ok (pas d'erreur) — le Conflict est terminal via mark_conflict.
    let out_conflict = handle_curate(
        conflict_job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate conflict doit retourner Ok (pas d'erreur handler) F-41");

    // Le résultat doit être vide (aucune note créée/modifiée).
    assert!(
        out_conflict.notes_created.is_empty(),
        "Conflict ne doit pas créer de note — out={out_conflict:?}"
    );
    assert!(
        out_conflict.notes_modified.is_empty(),
        "Conflict ne doit pas modifier de note — out={out_conflict:?}"
    );
    assert!(
        out_conflict.result_note_md.contains("conflict"),
        "result_note_md doit mentionner le conflit — out={out_conflict:?}"
    );

    // ── Vérification : note non écrasée (reste v2) ───────────────────────────
    let note_after_conflict = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("read_note après conflit F-41");
    assert!(
        note_after_conflict.body.markdown.contains("v2"),
        "note ne doit PAS être écrasée par v3 sur Conflict — body={:?}",
        note_after_conflict.body.markdown
    );
    assert!(
        !note_after_conflict.body.markdown.contains("v3"),
        "v3 ne doit PAS être dans le body après Conflict"
    );

    // ── Vérification : job marqué Conflict dans le store ────────────────────
    // Le store doit avoir été mis à jour par mark_conflict.
    // Note : mark_conflict est appelé via queue.mark_conflict() dans handle_curate,
    // mais le job_id passé au store est conflict_job_id (le job record id).
    // Le SqliteQueueStore fait un UPDATE sur ce job_id — mais le job n'est pas
    // persisté dans le store in-memory avant d'être traité (pas d'enqueue).
    // On vérifie donc le comportement observable : result_note_md contient "conflit"
    // ET la note n'est pas écrasée.
    //
    // Note complémentaire : le test de mark_conflict SQL complet est dans
    // gradatum-db-sqlite (unit tests queue_store_sqlite.rs).
    // Ce test vérifie la plomberie end-to-end handle_curate → résultat/note.

    // Extraire current_sha256 depuis result_note_md (format texte).
    let current_sha256_in_msg = ContentHash(hash_v2).hex();
    assert!(
        out_conflict.result_note_md.contains(&current_sha256_in_msg),
        "result_note_md doit contenir le current_sha256 de v2 — msg={:?}, expected_hash={}",
        out_conflict.result_note_md,
        current_sha256_in_msg
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1bis — GOLDEN F-41 : seam handler→ack→complete (régression LIVE 0.4.8)
// ─────────────────────────────────────────────────────────────────────────────

/// Golden anti-régression du bug LIVE F-41 (finding `debug:01KV2X3Z40VS7P998A2089CTGG`).
///
/// Le Test 1 ci-dessus valide `handle_curate` en isolation (note non écrasée,
/// `result_note_md` mentionne le conflit) MAIS ne traverse PAS le chemin
/// d'acknowledgement apalis. Or le bug vivait exactement dans ce seam : le handler
/// marque `Conflict` via `mark_conflict`, retourne `Ok(JobOutput)`, et l'ack apalis
/// (qui interprète `Ok` comme un succès) appelait `store.complete()` → écrasait
/// `Conflict` par `Done`.
///
/// Ce test reproduit le flux LIVE complet :
/// 1. `create` (sha frais absent) → note v1, status final `Done`.
/// 2. `update` avec sha v1 FRAIS → note v2 (corps changé), status final `Done`.
/// 3. `update` avec sha v1 PÉRIMÉ (courant = v2) → `handle_curate` marque `Conflict`,
///    puis on appelle `complete()` **comme le fait l'ack apalis sur `Ok`** :
///    le status final DOIT rester `Conflict` (garde anti-clobber) et le corps
///    DOIT rester v2 (note inchangée).
///
/// Sans la garde dans `QueueStore::complete`, l'étape 3 finirait `Done` → ce test échoue.
#[tokio::test]
async fn golden_f41_ack_complete_preserves_conflict_status() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn gradatum_core::QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let prealloc = Ulid::new();

    // ── Helper : exécute le seam complet handler → ack(complete sur Ok) ─────────
    // Reproduit `GradatumAcknowledger::ack` : sur `Ok(JobOutput)` → `store.complete`.
    // On enqueue d'abord le job (sinon `complete`/`mark_conflict` ne trouvent pas la
    // ligne par id) puis on lance le handler et on simule l'ack.
    async fn run_seam(
        fixture: &CurateFixture,
        store: &Arc<SqliteQueueStore>,
        queue: &Arc<dyn gradatum_core::QueueStore + Send + Sync>,
        curator: &Arc<gradatum_curator::CuratorPipeline>,
        prealloc: Ulid,
        body: &str,
        expected_sha256: Option<[u8; 32]>,
    ) -> (Ulid, JobStatus) {
        let job = curate_job_with_hash(prealloc, body, expected_sha256);
        let job_id = job.record.id;
        // Persister le job dans le store pour que complete/mark_conflict le retrouvent.
        store
            .enqueue(job.record.clone())
            .await
            .expect("enqueue job F-41 golden");

        // P2-6 (v3) : le handler et l'ack simulent le cycle Apalis complet.
        // dequeue → handler → complete. Sans le dequeue, le statut SQL reste
        // Pending et `complete()` est rejeté par le SELECT `WHERE status = 'Running'`.
        let dequeued = store
            .dequeue_by_kind("Curate", None)
            .await
            .expect("dequeue F-41 golden")
            .expect("le job doit être dequeuable");
        assert_eq!(dequeued.id, job_id, "dequeue doit retourner le bon job");

        let out = handle_curate(
            job,
            Data::new(Arc::new(TestInternalClient::new(
                Arc::clone(&fixture.vault),
                Arc::clone(&fixture.index),
            )) as Arc<dyn InternalClient>),
            Data::new(
                Arc::clone(curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>
            ),
            Data::new(Arc::clone(queue)),
            Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
        )
        .await
        .expect("handle_curate F-41 golden");

        // Simulation FIDÈLE de l'ack apalis : le handler a retourné Ok → l'ack
        // appelle store.complete avec un JobResult succès.
        //
        // P2-6 (v3) : le SELECT vérifie `status = 'Running'` — un job Conflict
        // n'est pas Running, donc `complete()` retourne `Err(NotFound)`.
        // C'est un comportement CORRECT : le job n'est pas en état d'être
        // complété, et son statut Conflict est préservé au niveau SQL.
        let ack_result = JobResult {
            success: true,
            duration_ms: 0,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        let complete_result = store.complete(job_id, ack_result).await;
        // P2-6 (v3) : le SELECT `WHERE status = 'Running'` ne trouve plus le job
        // Conflict → `Err(NotFound)`. Avant P2-6, la garde F-41 BLOB rendait
        // `Ok(())`. Les deux comportements préservent le statut Conflict.
        // Le test accepte les deux — ce qui compte, c'est que le statut final
        // soit bien Conflict (vérifié par l'appelant via status3).
        let _ = complete_result;

        let _ = out;
        let status = store
            .get(job_id, None)
            .await
            .expect("get job F-41 golden")
            .expect("job présent F-41 golden")
            .lifecycle
            .status;
        (job_id, status)
    }

    // ── Étape 1 : create (pas d'expected_sha256) → Done ───────────────────────
    let (_id1, status1) = run_seam(
        &fixture,
        &store,
        &queue,
        &curator,
        prealloc,
        "# v1\nbody v1.",
        None,
    )
    .await;
    assert_eq!(
        status1,
        JobStatus::Done,
        "create sans expected_sha256 doit finir Done"
    );
    let hash_v1 = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("read v1 golden")
        .content_hash
        .0;

    // ── Étape 2 : update avec sha v1 FRAIS → Done + corps = v2 ─────────────────
    let (_id2, status2) = run_seam(
        &fixture,
        &store,
        &queue,
        &curator,
        prealloc,
        "# v2\nbody v2 différent.",
        Some(hash_v1),
    )
    .await;
    assert_eq!(
        status2,
        JobStatus::Done,
        "update avec sha frais doit finir Done (écriture appliquée)"
    );
    let note_v2 = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("read v2 golden");
    assert!(
        note_v2.body.markdown.contains("v2"),
        "le corps doit être v2 après update sha-frais"
    );
    let hash_v2 = note_v2.content_hash.0;
    assert_ne!(hash_v1, hash_v2, "hash v1 ≠ hash v2");

    // ── Étape 3 : update avec sha v1 PÉRIMÉ (courant = v2) → Conflict + v2 ─────
    // C'EST le cœur du golden : avant le fix, le status finissait Done ici.
    let (_id3, status3) = run_seam(
        &fixture,
        &store,
        &queue,
        &curator,
        prealloc,
        "# v3 (DOIT être bloqué)\nbody v3.",
        Some(hash_v1), // périmé
    )
    .await;
    assert_eq!(
        status3,
        JobStatus::Conflict,
        "update avec sha PÉRIMÉ doit finir Conflict (PAS Done) — \
         la garde anti-clobber (F-41 BLOB + P2-6 SQL) protège l'état terminal"
    );

    // Note INCHANGÉE : v3 jamais appliqué, corps reste v2.
    let note_after = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("read après conflit golden");
    assert!(
        note_after.body.markdown.contains("v2"),
        "corps doit rester v2 après Conflict — body={:?}",
        note_after.body.markdown
    );
    assert!(
        !note_after.body.markdown.contains("v3"),
        "v3 ne doit JAMAIS être appliqué sur Conflict"
    );

    // Le conflict_payload doit porter le current_sha256 (= v2) pour résolution RMW.
    let result = store
        .get(_id3, None)
        .await
        .expect("get conflict job golden")
        .expect("job présent golden")
        .lifecycle
        .result
        .expect("result présent sur Conflict golden");
    let payload = result
        .conflict_payload
        .expect("conflict_payload présent sur Conflict golden");
    assert_eq!(
        payload.get("current_sha256").and_then(|v| v.as_str()),
        Some(ContentHash(hash_v2).hex().as_str()),
        "conflict_payload.current_sha256 doit être le hash v2 courant"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 — Sans expected_sha256 → écriture normale (rétrocompat)
// ─────────────────────────────────────────────────────────────────────────────

/// Sans `expected_sha256`, le job Curate écrit normalement — rétrocompat garantie.
#[tokio::test]
async fn handle_curate_no_expected_sha_writes_normally() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn gradatum_core::QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let prealloc = Ulid::new();
    let job = curate_job_with_hash(
        prealloc,
        "## Corps sans expected_sha256\nbody suffisant.",
        None, // pas de expected_sha256
    );

    let out = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate sans expected_sha256 F-41");

    assert!(
        !out.notes_created.is_empty(),
        "sans expected_sha256, la note doit être créée — out={out:?}"
    );
    let note = fixture.vault.read_note(NoteId(prealloc)).await;
    assert!(
        note.is_ok(),
        "note doit être lisible après écriture sans expected_sha256"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3 — Hash correct → écriture réussie
// ─────────────────────────────────────────────────────────────────────────────

/// Avec `expected_sha256` correct (= hash courant de la note), l'écriture réussit.
#[tokio::test]
async fn handle_curate_correct_hash_writes_normally() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn gradatum_core::QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let prealloc = Ulid::new();

    // Écriture initiale sans hash → obtenir hash_v1.
    let job_v1 = curate_job_with_hash(prealloc, "# Corps initial\nbody initial.", None);
    handle_curate(
        job_v1,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate v1 correct-hash F-41");

    let note_v1 = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("read_note v1 correct-hash F-41");
    let hash_v1 = note_v1.content_hash.0;

    // Écriture v2 avec le hash_v1 correct → doit réussir.
    let job_v2 = curate_job_with_hash(
        prealloc,
        "# Corps v2 correct\nbody v2 après hash correct.",
        Some(hash_v1), // hash correct
    );

    let out_v2 = handle_curate(
        job_v2,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate v2 avec hash correct F-41");

    assert!(
        !out_v2.notes_created.is_empty() || !out_v2.notes_modified.is_empty(),
        "hash correct doit permettre l'écriture — out={out_v2:?}"
    );
    assert!(
        !out_v2.result_note_md.contains("conflict"),
        "hash correct ne doit PAS produire un conflit — msg={:?}",
        out_v2.result_note_md
    );

    let note_v2 = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("read_note v2 correct-hash F-41");
    assert!(
        note_v2.body.markdown.contains("v2"),
        "note doit contenir v2 après écriture avec hash correct"
    );
}
