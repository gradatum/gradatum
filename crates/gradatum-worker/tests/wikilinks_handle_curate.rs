//! Tests TDD P0 — handle_curate renseigne persist_req.links (B5 wikilinks).
//!
//! Ce fichier teste que `handle_curate` (le handler Apalis actif, distinct du
//! `Dispatcher` legacy) résout et persiste les wikilinks `[[...]]` via le champ
//! `PersistCuratedRequest.links`, POUR LES DEUX branches Admitted ET Pending.
//!
//! # Protocole B5 attendu
//!
//! 1. `handle_curate` extrait les `[[...]]` du body.
//! 2. Appelle `client.title_lookup` pour chaque cible.
//! 3. Renseigne `persist_req.links` avec les `LinkDto` résolus.
//! 4. Le `TestInternalClient` exécute `index.upsert_link` sur chaque entrée de `.links`.
//! 5. Assertion : `index.backlinks(dst)` est non-vide.
//!
//! # Avant le fix
//!
//! `handle_curate` appelle `process_wikilinks_b5` (no-op) et passe `links: vec![]`
//! au `persist_curated` → les tests ci-dessous ÉCHOUENT avant le fix.

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;
use std::time::Duration;

use apalis::prelude::Data;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobFilter, JobLifecycle, JobLineage, JobMode,
    JobPriority, JobRecord, JobResult, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus,
    QueueError, QueueEvent, QueueStore, TriggerSource,
};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use tempfile::TempDir;
use tokio::sync::broadcast::Receiver;
use ulid::Ulid;

use test_internal_client::TestInternalClient;

// ─────────────────────────────────────────────────────────────────────────────
// NoopQueueStore — mock minimal pour handle_curate (enqueue best-effort)
// ─────────────────────────────────────────────────────────────────────────────

/// Mock `QueueStore` dont `enqueue()` retourne Ok (succès silencieux).
///
/// Utilisé pour isoler le test B5 : on ne veut pas tester le chaînage embed,
/// seulement la résolution des wikilinks dans persist_req.links.
struct NoopQueueStore;

#[async_trait]
impl QueueStore for NoopQueueStore {
    async fn enqueue(&self, _job: JobRecord) -> Result<Ulid, QueueError> {
        Ok(Ulid::new())
    }
    async fn dequeue(&self) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }
    async fn get(&self, _id: Ulid) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }
    async fn complete(&self, _id: Ulid, _result: JobResult) -> Result<(), QueueError> {
        Ok(())
    }
    async fn fail(&self, _id: Ulid, _err: &str, _attempt: u32) -> Result<(), QueueError> {
        Ok(())
    }
    async fn cancel(&self, _id: Ulid) -> Result<(), QueueError> {
        Ok(())
    }
    async fn fail_dlq(&self, _id: Ulid, _err: &str) -> Result<(), QueueError> {
        Ok(())
    }
    async fn find_awaiting(&self, _job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        Ok(())
    }
    async fn recover_stale_leases(&self, _ttl: Duration) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn cancel_expired_deadlines(
        &self,
        _now: chrono::DateTime<Utc>,
    ) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn promote_retries(&self, _now: chrono::DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn schedule_retry(
        &self,
        _id: Ulid,
        _at: chrono::DateTime<Utc>,
    ) -> Result<(), QueueError> {
        Ok(())
    }
    async fn list(&self, _filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    fn subscribe(&self) -> Receiver<QueueEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fixture
// ─────────────────────────────────────────────────────────────────────────────

struct HandleCurateFixture {
    _tmp: TempDir,
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    client: Arc<TestInternalClient>,
    curator: Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>,
    queue: Arc<dyn QueueStore + Send + Sync>,
}

async fn make_fixture() -> HandleCurateFixture {
    let tmp = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index = vault.index().clone();
    let client = Arc::new(TestInternalClient::new(vault.clone(), index.clone()));
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let curator: Arc<dyn gradatum_curator::CuratorProcess + Send + Sync> =
        Arc::new(gradatum_curator::CuratorPipeline::new());

    HandleCurateFixture {
        _tmp: tmp,
        vault,
        index,
        client,
        curator,
        queue,
    }
}

/// Seed une note Live dans le vault+index. Retourne son ULID stringifié.
async fn seed_note(fixture: &HandleCurateFixture, title: &str, body: &str) -> String {
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let full_body = format!("# {title}\n{body}");
    let note = fixture
        .vault
        .write_note(fm, full_body)
        .await
        .expect("seed write_note");
    fixture
        .index
        .upsert_note_title(&note.id, title)
        .await
        .expect("seed upsert_note_title");
    note.id.to_string()
}

/// Construit un GradatumJob::Curate en mode Batch avec title+body.
fn make_curate_job(title: &str, body: &str, section_hint: Option<&str>) -> GradatumJob {
    let now = Utc::now();
    let note_id_ulid = Ulid::new();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id: note_id_ulid,
                    tenant_id: "main".to_string(),
                    title: Some(title.to_string()),
                    body: Some(body.to_string()),
                    tags: vec![],
                    section_hint: section_hint.map(|s| s.to_string()),
                    author: None,
                    expected_sha256: None,
                    occurred_at: None,
                }),
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::Normal,
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
// Test P0-A : handle_curate Admitted persiste les liens
// ─────────────────────────────────────────────────────────────────────────────

/// P0-A : handle_curate (Admitted) doit renseigner persist_req.links avec
/// les wikilinks résolus, ce qui provoque upsert_link dans TestInternalClient.
///
/// Avant le fix : `process_wikilinks_b5` est no-op, `links: vec![]` → FAIL.
/// Après le fix  : `resolve_wikilinks_via_client` renseigne `links` → PASS.
#[tokio::test]
async fn handle_curate_admitted_persists_wikilinks_in_links_field() {
    let fixture = make_fixture().await;

    // Seed la note cible
    let target_title = "Note Cible HandleCurate Admitted";
    let target_id = seed_note(
        &fixture,
        target_title,
        "Contenu de référence pour le test P0-A.",
    )
    .await;

    // Body avec wikilink vers la note cible
    // Préfixe [DECISIONS] → curator heuristique → Admitted
    let body = format!(
        "# [DECISIONS] Test wikilinks handle_curate\n\n\
         Voir [[{target_title}]] pour le contexte de la décision."
    );
    let job = make_curate_job(
        "[DECISIONS] Test wikilinks handle_curate",
        &body,
        Some("decisions"),
    );

    let client_data =
        Data::new(Arc::clone(&fixture.client)
            as Arc<dyn gradatum_worker::internal_client::InternalClient>);
    let curator_data = Data::new(Arc::clone(&fixture.curator));
    let queue_data = Data::new(Arc::clone(&fixture.queue));

    let result =
        gradatum_worker::apalis_handlers::handle_curate(job, client_data, curator_data, queue_data)
            .await;
    assert!(
        result.is_ok(),
        "handle_curate ne doit pas échouer — err={result:?}"
    );

    // Assertion B5 : la note cible doit avoir au moins un backlink
    let backs = fixture
        .index
        .backlinks("main", &target_id)
        .await
        .expect("backlinks");
    assert!(
        !backs.is_empty(),
        "P0-A : handle_curate (Admitted) doit persister le wikilink via links field. \
         backlinks vers {target_id} = {backs:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test P0-B : handle_curate Pending persiste les liens
// ─────────────────────────────────────────────────────────────────────────────

/// P0-B : handle_curate (Pending) doit aussi renseigner persist_req.links.
///
/// Parité Admitted/Pending — les deux branches doivent résoudre les wikilinks.
/// Avant le fix : `links: vec![]` dans la branche Pending → FAIL.
/// Après le fix  : `resolve_wikilinks_via_client` appelée pour Pending → PASS.
///
/// Note : déclencher un vrai Pending via CuratorPipeline est aléatoire selon
/// l'heuristique. On utilise donc un titre sans préfixe dominant et un body court
/// pour obtenir une confidence basse → Pending, ou Admitted. Si Admitted,
/// le test reste valide car B5 doit fonctionner dans les deux cas.
#[tokio::test]
async fn handle_curate_pending_persists_wikilinks_in_links_field() {
    let fixture = make_fixture().await;

    let target_title = "Note Cible HandleCurate Pending";
    let target_id = seed_note(
        &fixture,
        target_title,
        "Contenu de référence pour le test P0-B.",
    )
    .await;

    let body = format!(
        "# Brouillon en attente\n\n\
         Voir [[{target_title}]] — note en attente de classification."
    );
    // Titre court sans préfixe → confidence basse → Pending ou Admitted
    let job = make_curate_job("Brouillon", &body, None);

    let client_data =
        Data::new(Arc::clone(&fixture.client)
            as Arc<dyn gradatum_worker::internal_client::InternalClient>);
    let curator_data = Data::new(Arc::clone(&fixture.curator));
    let queue_data = Data::new(Arc::clone(&fixture.queue));

    let result =
        gradatum_worker::apalis_handlers::handle_curate(job, client_data, curator_data, queue_data)
            .await;
    assert!(
        result.is_ok(),
        "handle_curate (Pending path) ne doit pas échouer — err={result:?}"
    );

    // Assertion B5 : le wikilink doit être persisté quelle que soit la branche
    let backs = fixture
        .index
        .backlinks("main", &target_id)
        .await
        .expect("backlinks");
    assert!(
        !backs.is_empty(),
        "P0-B : handle_curate (Pending ou Admitted) doit persister le wikilink. \
         backlinks vers {target_id} = {backs:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test P0-C : wikilink non résolu non-fatal
// ─────────────────────────────────────────────────────────────────────────────

/// P0-C : un wikilink vers une note inexistante ne doit pas faire échouer handle_curate.
///
/// Comportement attendu : le job réussit, aucun lien fantôme créé.
#[tokio::test]
async fn handle_curate_unresolved_wikilink_is_non_fatal() {
    let fixture = make_fixture().await;

    let body = "# [DECISIONS] Note avec wikilink cassé\n\n\
                Voir [[Note Totalement Inexistante XYZ789]] — wikilink non résolu.";
    let job = make_curate_job(
        "[DECISIONS] Note avec wikilink cassé",
        body,
        Some("decisions"),
    );

    let client_data =
        Data::new(Arc::clone(&fixture.client)
            as Arc<dyn gradatum_worker::internal_client::InternalClient>);
    let curator_data = Data::new(Arc::clone(&fixture.curator));
    let queue_data = Data::new(Arc::clone(&fixture.queue));

    let result =
        gradatum_worker::apalis_handlers::handle_curate(job, client_data, curator_data, queue_data)
            .await;
    assert!(
        result.is_ok(),
        "P0-C : wikilink non résolu ne doit pas faire échouer handle_curate — err={result:?}"
    );
}
