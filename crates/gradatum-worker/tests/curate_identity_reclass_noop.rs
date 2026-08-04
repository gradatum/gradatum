//! Test P1-A (audit reviewer v0.7.3) — reclassification d'une note `identity` doit être un no-op.
//!
//! ## Problème (P1-A)
//!
//! Le chemin reclassification de `handle_curate` (`title=None, body=None`) lit la note
//! existante via `InternalClient::get_note`, en dérive un `CuratorNote` depuis le corps
//! de l'âme, puis laisse le curator décider d'une nouvelle section canonique.
//! Pour une note `section=identity`, cela peut :
//! 1. Modifier la section (changement ACL — la note sort de la section protégée).
//! 2. Clobber le `title` canonique (`identity/<agent>`) par le H1 extrait par
//!    `extract_h1_title`, qui peut différer selon le corpus.
//!
//! ## Fix (P1-A)
//!
//! Après lecture de `existing_dto` dans la branche reclassification,
//! si `existing_dto.section == "identity"` → retourner un `JobOutput` no-op
//! (notes_created/modified vides, `result_note_md` explicatif) sans appeler
//! le curator ni écrire dans le vault.
//!
//! ## Ce que ce test vérifie
//!
//! 1. `handle_curate` sur une note `identity` en mode reclassification retourne `Ok(output)`.
//! 2. `output.notes_created` et `output.notes_modified` sont tous deux vides.
//! 3. La note conserve `section=identity` après l'exécution du job.
//! 4. Le `title` canonique de l'âme est inchangé (`identity/test-agent`).

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
    frontmatter::{ExtraFields, Frontmatter},
    identity::NoteId,
    scope::VaultId,
    section::Section,
    status::NoteStatus,
};
use gradatum_db_sqlite::{SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::handle_curate;
use gradatum_worker::internal_client::InternalClient;
use test_internal_client::TestInternalClient;

use smallvec::SmallVec;
use sqlx::SqlitePool;
use tempfile::TempDir;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

async fn test_store() -> SqliteQueueStore {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("pool in-memory");
    apply_sqlite_pragmas(&pool).await.expect("pragmas");
    run_migrations(&pool).await.expect("migrations");
    SqliteQueueStore::new(pool)
}

struct IdentityFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    /// ULID de la note identity créée au setup.
    note_id: Ulid,
    _tmp: TempDir,
}

impl IdentityFixture {
    /// Crée un vault temporaire et y insère une note `section=identity` (âme minimale).
    async fn new() -> Self {
        let tmp = TempDir::new().expect("TempDir");
        let vault = Arc::new(
            Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
                .await
                .expect("Vault::create"),
        );
        let index = vault.index().clone();
        let note_id = Ulid::new();

        // Corps d'âme minimal — titre `identity/test-agent`.
        let body =
            "# identity/test-agent\nextends: identity/main\n\n## NARRATIVE\nTu es Test Agent.\n";

        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section: Section::Identity,
            status: NoteStatus::Live,
            status_reason: None,
            status_changed: None,
            tags: SmallVec::new(),
            author: None,
            created: Utc::now(),
            updated: None,
            extra: ExtraFields::empty(),
            provenance: Some("test".to_string()),
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };

        vault
            .write_note_with_id(frontmatter, body.to_string(), NoteId(note_id))
            .await
            .expect("write identity note");

        // Indexer la note pour que le chemin reclassification puisse la lire.
        index
            .upsert_note_title("main", &NoteId(note_id), "identity/test-agent")
            .await
            .expect("upsert_note_title");

        IdentityFixture {
            vault,
            index,
            note_id,
            _tmp: tmp,
        }
    }
}

/// Construit un job curate de reclassification (title + body absents) sur la note donnée.
fn curate_job_reclassify(note_id: Ulid, tenant_id: &str) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id,
                    tenant_id: tenant_id.to_string(),
                    // Reclassification path : title + body absents
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

/// P1-A : reclassification d'une note `section=identity` → no-op.
///
/// Vérifie que `handle_curate` sur une note identity en mode reclassification
/// retourne un `JobOutput` vide (no-op) sans modifier la section ni le title.
///
/// Régression potentielle avant le fix : le curator aurait pu router l'âme vers
/// une section non-identity (ex: `reference`), changeant l'ACL et corrompant
/// le title canonique `identity/<agent>`.
#[tokio::test]
async fn identity_reclass_is_noop() {
    let fixture = IdentityFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn gradatum_core::QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let job = curate_job_reclassify(fixture.note_id, "main");

    let output = handle_curate(
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
    .expect("handle_curate identity reclassify doit retourner Ok");

    // P1-A invariant 1 : no-op → notes_created et notes_modified vides.
    assert!(
        output.notes_created.is_empty(),
        "P1-A : notes_created doit être vide pour une reclassification identity — output={output:?}"
    );
    assert!(
        output.notes_modified.is_empty(),
        "P1-A : notes_modified doit être vide pour une reclassification identity — output={output:?}"
    );

    // P1-A invariant 2 : la note est toujours lisible à son ULID d'origine.
    let stored = fixture
        .vault
        .read_note(NoteId(fixture.note_id))
        .await
        .expect("P1-A : la note identity doit rester lisible après la reclassification no-op");

    // P1-A invariant 3 : la section reste `identity`.
    assert_eq!(
        stored.frontmatter.section,
        Section::Identity,
        "P1-A : la section doit rester identity après la reclassification no-op"
    );

    // P1-A invariant 4 : le title canonique est préservé.
    // (lu depuis le corps de la note — colonne index non vérifiée ici)
    assert!(
        stored.body.markdown.contains("identity/test-agent"),
        "P1-A : le title canonique 'identity/test-agent' doit être préservé dans le corps"
    );
}
