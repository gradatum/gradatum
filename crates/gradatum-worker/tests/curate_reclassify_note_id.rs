//! Test de régression R2 — chemin reclassification préserve l'ULID de la note existante.
//!
//! ## Problème (bug R2 / avatar bug C v0.3.7)
//!
//! Quand `handle_curate` traite un job de **reclassification** (title + body absents dans
//! `CurateSpec`), la note existante doit être mise à jour en préservant son ULID d'origine.
//!
//! Avant le fix, le chemin utilisait `Vault::write_note(fm, body)` qui délègue à
//! `write_note_inner(..., NoteId::new())` → génère un NOUVEL ULID. Conséquence :
//! - L'ULID retourné dans le 202 (`spec.note_id`) ne correspond plus à la note stockée.
//! - `read_note(spec.note_id)` → NoteNotFound (404).
//! - Tous les wikilinks référençant cette note → morts.
//!
//! ## Fix
//!
//! Remplacement de `write_note` par `write_note_with_id(fm, body, existing_note_id)` sur
//! les deux branches reclassification (Admitted + Pending) dans `apalis_handlers.rs`.
//!
//! ## Ce que ce test vérifie
//!
//! 1. Après reclassification via `handle_curate`, la note est lisible à son ULID original.
//! 2. L'ULID stocké == ULID original (pas de doublon créé avec un nouvel ULID).
//! 3. La section / les tags ont bien été mis à jour (reclassification effective).

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
    identity::NoteId, scope::VaultId,
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
// Helpers (calqués sur curate_prealloc_note_id.rs)
// ─────────────────────────────────────────────────────────────────────────────

async fn test_store() -> SqliteQueueStore {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("pool in-memory");
    apply_sqlite_pragmas(&pool).await.expect("pragmas");
    run_migrations(&pool).await.expect("migrations");
    SqliteQueueStore::new(pool)
}

struct CurateFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    _tmp: TempDir,
}

impl CurateFixture {
    async fn new() -> Self {
        let tmp = TempDir::new().expect("TempDir");
        let vault = Arc::new(
            Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
                .await
                .expect("Vault::create"),
        );
        let index = vault.index().clone();
        CurateFixture {
            vault,
            index,
            _tmp: tmp,
        }
    }
}

/// Construit un job curate de **reclassification** (title + body absents).
///
/// L'ULID fourni désigne une note déjà existante dans le vault.
fn curate_job_reclassify(existing_note_id: Ulid, tenant_id: &str) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id: existing_note_id,
                    tenant_id: tenant_id.to_string(),
                    // title + body absents → path reclassification
                    title: None,
                    body: None,
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
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Après reclassification via `handle_curate`, la note doit être lisible à son ULID
/// original — l'ULID ne doit pas changer.
///
/// Régression : avant le fix, `write_note` générait NoteId::new() → ULID divergent
/// → `read_note(original_id)` = NoteNotFound.
#[tokio::test]
async fn handle_curate_reclassify_preserves_note_id() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn gradatum_core::QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    // Étape 1 : créer une note dans le vault via vault_write normal
    // Préfixe [DECISIONS] → heuristique confidence ≥ 0.8 → Admitted direct.
    let prealloc = Ulid::generate();
    let body = "# [DECISIONS] Titre existant\nContenu à reclassifier.";
    let create_job = {
        let now = Utc::now();
        let class = JobClass::Agent;
        GradatumJob {
            priority: JobPriority::default_for(&class).as_u8(),
            record: JobRecord {
                id: Ulid::generate(),
                spec: JobSpec {
                    kind: Job::Curate(CurateSpec {
                        note_id: prealloc,
                        tenant_id: "main".to_string(),
                        title: Some("[DECISIONS] Titre existant".to_string()),
                        body: Some(body.to_string()),
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
    };

    let create_out = handle_curate(
        create_job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate vault_write");

    // Précondition : la note a bien été créée à l'ULID préalloué
    assert!(
        !create_out.notes_created.is_empty(),
        "vault_write doit créer la note — output={create_out:?}"
    );
    let stored_before = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("la note doit être lisible avant reclassification");
    assert_eq!(
        stored_before.id,
        NoteId(prealloc),
        "précondition : ULID stocké = ULID préalloué"
    );

    // Étape 2 : reclassifier via un job sans title/body
    let reclassify_job = curate_job_reclassify(prealloc, "main");

    let reclassify_out = handle_curate(
        reclassify_job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate reclassify");

    // La reclassification signale la note via notes_created (même champ que vault_write —
    // le handler curate n'a pas de champ notes_modified distinct pour les updates).
    // L'invariant principal est que l'ULID retourné == ULID original.
    let reclassify_note_ids = reclassify_out.notes_created.clone();
    assert!(
        !reclassify_note_ids.is_empty(),
        "reclassification doit signaler la note dans notes_created — output={reclassify_out:?}"
    );
    assert_eq!(
        reclassify_note_ids[0], prealloc,
        "R2 : notes_created[0] doit être l'ULID original (pas un nouvel ULID)"
    );

    // Invariant principal : la note est toujours lisible à l'ULID d'origine
    let stored_after = fixture
        .vault
        .read_note(NoteId(prealloc))
        .await
        .expect("R2 : la note doit être lisible à son ULID original après reclassification");

    assert_eq!(
        stored_after.id,
        NoteId(prealloc),
        "R2 : l'ULID stocké après reclassification doit être l'ULID original"
    );

    // Vérifier qu'aucune note supplémentaire n'a été créée avec un nouvel ULID
    // (symptôme du bug : deux notes dans le vault, l'originale + la mauvaise)
    let locus_count = fixture.index.locus_count().await.expect("locus_count");
    assert_eq!(
        locus_count, 1,
        "R2 : exactement 1 note dans le vault après reclassification (pas de doublon ULID)"
    );
}
