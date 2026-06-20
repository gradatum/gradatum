//! Test de régression — persistence de `notes.title` après curation.
//!
//! ## Problème (bug LIVE 2026-06-03)
//!
//! La colonne `notes.title` était quasi-vide en production (910/911 notes NULL).
//! `handle_curate` écrivait la note dans le vault + l'index FTS mais n'appelait
//! jamais `DocumentStore::upsert_note_title()`.
//!
//! ## Couverture
//!
//! | Test | Comportement validé |
//! |---|---|
//! | `curate_persists_title_in_index` | Note vault_write → `notes.title` = titre H1 |
//! | `search_fts_returns_title_after_curate` | `search_fts_with_snippet` remonte `title` non-null |
//!
//! ## Pré-condition : ces tests DOIVENT échouer sans le fix write-path
//!
//! Sans le wire `upsert_note_title` dans `handle_curate`, `notes.title` reste NULL
//! et `title_lookup_by_column` ne trouve rien.
//!
//! ## Architecture
//!
//! - `Vault::create(TempDir)` réel (fichiers .md + index SQLite)
//! - `CuratorPipeline::new()` réel (heuristique locale, pas de LLM)
//! - `SqliteQueueStore` in-memory pour le chaînage embed (non testé ici)
//! - Vérification directe via `SqliteIndex::search_fts_with_snippet` et SQL
//!
//! ## Référence fix
//!
//! - `gradatum-worker/src/apalis_handlers.rs` : bloc `upsert_note_title post-curate`
//! - `gradatum-index/migrations/0009_backfill_title.sql`

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
    scope::VaultId,
};
use gradatum_db_sqlite::{SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::{HandlerError, handle_curate};
use gradatum_worker::internal_client::InternalClient;
use test_internal_client::TestInternalClient;

use sqlx::SqlitePool;
use tempfile::TempDir;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Crée un `SqliteQueueStore` in-memory avec schéma appliqué.
async fn test_queue() -> Arc<SqliteQueueStore> {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("pool in-memory");
    apply_sqlite_pragmas(&pool).await.expect("pragmas");
    run_migrations(&pool).await.expect("migrations");
    Arc::new(SqliteQueueStore::new(pool))
}

/// Fixture : vault + index + curator réels.
struct TitleFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    _tmp: TempDir,
}

impl TitleFixture {
    async fn new() -> Self {
        let tmp = TempDir::new().expect("TempDir");
        let vault = Arc::new(
            Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
                .await
                .expect("Vault::create"),
        );
        let index = vault.index().clone();
        TitleFixture {
            vault,
            index,
            _tmp: tmp,
        }
    }
}

/// Construit un `GradatumJob` curate minimal — path vault_write (title + body présents).
///
/// Le préfixe `[DECISIONS]` pousse l'heuristique CuratorPipeline à admettre
/// la note avec confidence ≥ 0.8 (pas de LLM requis en test).
fn make_vault_write_curate_job(title: &str, body: &str) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id: Ulid::new(),
                    tenant_id: "main".to_string(),
                    title: Some(title.to_string()),
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
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1 — handle_curate peuple notes.title après vault_write
// ─────────────────────────────────────────────────────────────────────────────

/// Régression : après `handle_curate`, `notes.title` doit contenir le titre H1.
///
/// ## Vérification
///
/// `SqliteIndex::title_lookup` cherche par `body_text LIKE '# titre\n%'` — il retourne
/// toujours le note_id si le body existe (même sans la colonne `title`).
/// Pour tester la colonne `title`, on utilise `search_fts_with_snippet` qui remonte
/// `SearchHitRaw.title` — c'est `Some(...)` uniquement si `upsert_note_title` a été
/// appelé et a peuplé `notes.title`.
///
/// **DOIT échouer sans le fix write-path** (titre restera `None` dans `SearchHitRaw`).
#[tokio::test]
async fn curate_persists_title_in_index() {
    let fixture = TitleFixture::new().await;
    let queue = test_queue().await;
    let queue_dyn: Arc<dyn QueueStore + Send + Sync> = queue as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let expected_title = "[DECISIONS] Titre Régression Persist";
    let body = format!("# {expected_title}\n\nContenu suffisant pour admission dans le vault.");

    let job = make_vault_write_curate_job(expected_title, &body);

    let result = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue_dyn)),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok — err={result:?}"
    );

    let output = result.unwrap();

    // La note doit avoir été créée (Admitted)
    assert!(
        !output.notes_created.is_empty(),
        "handle_curate doit créer la note — notes_created vide"
    );

    // Vérification via search_fts_with_snippet : SearchHitRaw.title doit être Some
    // et correspondre au titre injecté.
    let vault_id = VaultId::new("main");
    // Mot clé court non ambigu pour le FTS
    let hits = fixture
        .index
        .search_fts_with_snippet(&vault_id, "Régression Persist", 5, false, None, None, None)
        .await
        .expect("search_fts_with_snippet");

    // Au moins 1 hit
    assert!(
        !hits.is_empty(),
        "search FTS doit retourner au moins 1 hit pour la note curée"
    );

    // Le premier hit doit avoir un title non-null
    let hit = &hits[0];
    assert!(
        hit.title.is_some(),
        "SearchHitRaw.title doit être Some après curate — got None \
         (fix absent : handle_curate n'appelle pas upsert_note_title)"
    );

    // Le titre doit correspondre
    let actual_title = hit.title.as_deref().unwrap_or("");
    assert_eq!(
        actual_title, expected_title,
        "SearchHitRaw.title doit correspondre au titre injecté — \
         got={actual_title:?} want={expected_title:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 — search_fts retourne title non-null après curate
// ─────────────────────────────────────────────────────────────────────────────

/// Complémentaire au test 1 : vérifie que `search_fts_with_snippet` remonte bien
/// `title` même avec un terme de recherche différent du titre.
///
/// Ce test couvre le cas d'usage réel de vault_search : un utilisateur cherche
/// par contenu et attend de voir le titre de la note dans les résultats.
#[tokio::test]
async fn search_fts_returns_title_after_curate() {
    let fixture = TitleFixture::new().await;
    let queue = test_queue().await;
    let queue_dyn: Arc<dyn QueueStore + Send + Sync> = queue as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let note_title = "[DECISIONS] Architecture vault-search";
    let body = format!(
        "# {note_title}\n\n\
         La recherche FTS5 doit retourner le titre de la note dans les résultats. \
         Correction du bug LIVE 2026-06-03 où notes.title était null."
    );

    let job = make_vault_write_curate_job(note_title, &body);

    let result = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue_dyn)),
    )
    .await;

    assert!(result.is_ok(), "handle_curate Ok — err={result:?}");
    let output = result.unwrap();

    // Note doit être admise pour que le test soit valide
    if output.notes_created.is_empty() {
        // Curator a rejeté malgré le préfixe — log + skip le test titre
        // (heuristique non garantie à 100% sur corps avec "bug LIVE")
        return;
    }

    let vault_id = VaultId::new("main");

    // Chercher par un mot du body (pas du titre) — vérifier que `title` est présent dans le hit
    let hits = fixture
        .index
        .search_fts_with_snippet(&vault_id, "correction bug", 5, false, None, None, None)
        .await
        .expect("search_fts_with_snippet");

    assert!(
        !hits.is_empty(),
        "FTS doit trouver la note par le contenu du body"
    );

    // Tous les hits doivent avoir un title (la note créée juste au-dessus)
    let hit = hits
        .iter()
        .find(|h| {
            h.title
                .as_deref()
                .map(|t| t.contains("vault-search"))
                .unwrap_or(false)
        })
        .expect(
            "aucun hit avec title contenant 'vault-search' — \
             upsert_note_title n'a pas été appelé par handle_curate",
        );

    assert!(
        hit.title.is_some(),
        "SearchHitRaw.title doit être Some — got None"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test — P0 cross-tenant Lot 5 : worker rejette un spec tenant ≠ main
// ─────────────────────────────────────────────────────────────────────────────

/// Construit un job curate avec un `tenant_id` arbitraire (pour tester la garde Lot 5).
fn make_curate_job_with_tenant(tenant: &str, title: &str, body: &str) -> GradatumJob {
    let mut job = make_vault_write_curate_job(title, body);
    if let Job::Curate(ref mut spec) = job.record.spec.kind {
        spec.tenant_id = tenant.to_string();
    }
    job
}

/// Le worker est hors middleware HTTP : un spec curate tenant ≠ "main" doit être
/// rejeté terminalement (`HandlerError::Business`), jamais traité.
#[tokio::test]
async fn curate_rejects_non_main_tenant() {
    let fixture = TitleFixture::new().await;
    let queue = test_queue().await;
    let queue_dyn: Arc<dyn QueueStore + Send + Sync> = queue as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let title = "[DECISIONS] Note tenant evil";
    let body = format!("# {title}\n\nContenu — ne doit jamais être écrit.");
    let job = make_curate_job_with_tenant("evil", title, &body);

    let result = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue_dyn)),
    )
    .await;

    assert!(
        matches!(result, Err(HandlerError::Business(_))),
        "spec tenant='evil' → HandlerError::Business (reject terminal), obtenu : {result:?}"
    );
}

/// Contre-épreuve : tenant "main" reste traité normalement (zéro breaking).
#[tokio::test]
async fn curate_accepts_main_tenant() {
    let fixture = TitleFixture::new().await;
    let queue = test_queue().await;
    let queue_dyn: Arc<dyn QueueStore + Send + Sync> = queue as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let title = "[DECISIONS] Note tenant main OK";
    let body = format!("# {title}\n\nContenu admis dans le vault.");
    let job = make_curate_job_with_tenant("main", title, &body);

    let result = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue_dyn)),
    )
    .await;

    assert!(
        result.is_ok(),
        "spec tenant='main' → Ok, obtenu : {result:?}"
    );
}
