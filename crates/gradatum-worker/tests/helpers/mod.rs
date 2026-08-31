//! Helpers tests partagés — worker B5 wikilinks (chemin ACTIF `handle_curate`).
//!
//! Pattern TDD : `#[path = "helpers/mod.rs"] mod helpers;` au début de chaque
//! fichier test d'intégration touchant les wikilinks du worker.
//!
//! Fournit :
//! - `MockInternalClient` — implémente `InternalClient` en appelant `Vault`/`SqliteIndex`
//!   directement en local (pas de HTTP). Permet aux tests d'intégration de continuer
//!   à fonctionner sans un serveur HTTP.
//! - `test_curate_fixture` — construit Vault + SqliteIndex + client mock + curator +
//!   `SqliteQueueStore` (le backend JSON du moteur actif Apalis).
//! - `process_curate` — construit un `GradatumJob::Curate` (title+body) et appelle
//!   `handle_curate` directement (moteur actif, `apalis_handlers`). Remplace l'ancien
//!   couple `enqueue_curate_job` + `Dispatcher::run_once` du moteur legacy supprimé.
//! - Helpers d'encodage et d'assertions B5 wikilinks.
//!
//! ## Architecture MockInternalClient
//!
//! - `persist_curated` → `vault.write_note_with_id(...)` + boucle sur `req.links` → `idx.upsert_link(...)`
//! - `persist_embedding` → `idx.insert_note_embedding(...)`
//! - `title_lookup` / `id_lookup` → `idx.title_lookup(...)` / `idx.id_lookup(...)`
//! - `get_note` → `vault.read_note(...)` + sérialisation vers `NoteReadDto`
//! - Autres méthodes : `unimplemented!()` (non utilisées par le chemin curate)

#![allow(dead_code)]

use std::sync::Arc;

use apalis::prelude::Data;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::VectorStore as _;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, TriggerSource,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::{HandlerError, MultiTenantCfg, handle_curate};
use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteIdDto, NoteReadDto,
};
use tempfile::TempDir;
use ulid::Ulid;

// ── MockInternalClient ────────────────────────────────────────────────────────

/// Mock `InternalClient` pour les tests d'intégration.
///
/// Délègue les mutations directement à `Vault` et `SqliteIndex` en local
/// (pas de HTTP). Permet à `handle_curate` d'opérer sans serveur HTTP.
pub struct MockInternalClient {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
}

impl MockInternalClient {
    pub fn new(vault: Arc<Vault>, index: Arc<SqliteIndex>) -> Self {
        Self { vault, index }
    }
}

#[async_trait]
impl InternalClient for MockInternalClient {
    // ── Writes ──

    async fn persist_curated(
        &self,
        req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        use chrono::Utc;
        use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
        use gradatum_core::section::Section;
        use gradatum_core::status::NoteStatus;
        use smallvec::SmallVec;

        // Parse section and status from kebab strings
        let section: Section =
            serde_json::from_str(&format!("\"{}\"", req.section)).unwrap_or(Section::Reference);
        let status: NoteStatus =
            serde_json::from_str(&format!("\"{}\"", req.status)).unwrap_or(NoteStatus::Live);

        let mut all_tags: SmallVec<[gradatum_core::tag::Tag; 4]> = SmallVec::new();
        for tag_str in &req.tags {
            if let Ok(t) = gradatum_core::tag::Tag::new(tag_str.clone()) {
                all_tags.push(t);
            }
        }

        let author = req
            .author
            .as_deref()
            .map(gradatum_core::author::AuthorRef::system);

        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new(req.tenant_id.as_str()),
            locus: None,
            section,
            status,
            status_reason: None,
            status_changed: None,
            tags: all_tags,
            author,
            created: Utc::now(),
            updated: None,
            extra: ExtraFields::empty(),
            provenance: None,
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };

        let note_id = req
            .note_id
            .parse::<Ulid>()
            .map(NoteId)
            .unwrap_or_else(|_| NoteId::new());

        // Write the note with the pre-allocated ID.
        let note = self
            .vault
            .write_note_with_id(frontmatter, req.body.clone(), note_id)
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: e.to_string(),
            })?;

        // Also upsert the note title into the index for title_lookup resolution.
        if !req.title.is_empty() {
            let _ = self
                .index
                .upsert_note_title(note.frontmatter.vault_id.as_str(), &note.id, &req.title)
                .await;
        }

        // Process links — upsert each (src→dst) pair into note_links.
        // Non-fatal: a failed upsert_link is logged but does not fail the persist.
        for link in &req.links {
            if let Err(e) = self
                .index
                .upsert_link(req.tenant_id.as_str(), &link.src, &link.dst)
                .await
            {
                tracing::warn!(
                    src = %link.src,
                    dst = %link.dst,
                    err = %e,
                    "MockInternalClient: upsert_link failed (non-fatal)"
                );
            }
        }

        Ok(PersistOkResponse {
            note_id: note.id.to_string(),
            status: "ok".to_string(),
        })
    }

    async fn persist_embedding(
        &self,
        req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError> {
        let note_ulid =
            Ulid::from_string(&req.note_id).map_err(|e| InternalClientError::ServerError {
                status: 400,
                body: format!("ULID invalide: {e}"),
            })?;
        let note_id = NoteId(note_ulid);

        self.index
            .insert_note_embedding("main", &note_id, &req.embedder_id, req.dim, &req.vector)
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: e.to_string(),
            })?;

        Ok(EmbeddingOkResponse {
            note_id: req.note_id.clone(),
            embedder_id: req.embedder_id.clone(),
            dim: req.dim as usize,
        })
    }

    async fn persist_forget(
        &self,
        _req: &PersistForgetRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("MockInternalClient::persist_forget not used by curate tests")
    }

    async fn persist_distill(
        &self,
        _req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("MockInternalClient::persist_distill not used by curate tests")
    }

    async fn delete_note(&self, _vault_id: &str, _ulid: &str) -> Result<(), InternalClientError> {
        unimplemented!("MockInternalClient::delete_note not used by curate tests")
    }

    // ── Reads ──

    async fn get_note(
        &self,
        _vault_id: &str,
        ulid: &str,
    ) -> Result<NoteReadDto, InternalClientError> {
        let note_ulid = Ulid::from_string(ulid).map_err(|_| InternalClientError::NotFound {
            ulid: ulid.to_string(),
        })?;
        let note_id = NoteId(note_ulid);

        let note =
            self.vault
                .read_note(note_id)
                .await
                .map_err(|_| InternalClientError::NotFound {
                    ulid: ulid.to_string(),
                })?;

        Ok(NoteReadDto {
            note_id: note.id.to_string(),
            sha256_hex: note.content_hash.hex(),
            body: note.body.markdown.clone(),
            section: note.frontmatter.section.as_str().to_string(),
            status: note.frontmatter.status.to_string(),
            tags: note
                .frontmatter
                .tags
                .iter()
                .map(|t| t.as_str().to_string())
                .collect(),
            forgotten: note.frontmatter.forgotten.unwrap_or(false),
            processed: false,
        })
    }

    async fn get_note_status(
        &self,
        vault_id: &str,
        ulid: &str,
    ) -> Result<Option<String>, InternalClientError> {
        // Délégation à l'index réel (scopé `WHERE vault_id AND id`), cohérent prod.
        self.index
            .get_note_status(vault_id, ulid)
            .await
            .map(|opt| opt.map(|s| s.to_string()))
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })
    }

    async fn get_note_embedding(
        &self,
        _vault_id: &str,
        _ulid: &str,
        _embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        unimplemented!("MockInternalClient::get_note_embedding not used by curate tests")
    }

    async fn get_trust(&self, _vault_id: &str, _ulid: &str) -> Result<f32, InternalClientError> {
        unimplemented!("MockInternalClient::get_trust not used by curate tests")
    }

    async fn title_lookup(
        &self,
        tenant: &str,
        title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        self.index
            .title_lookup(tenant, title)
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: e.to_string(),
            })
    }

    async fn id_lookup(
        &self,
        tenant: &str,
        note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        self.index
            .id_lookup(tenant, note_id)
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: e.to_string(),
            })
    }

    async fn list_notes_by_locus(
        &self,
        _vault: &str,
        _prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::list_notes_by_locus not used by curate tests")
    }

    async fn list_by_status(
        &self,
        _vault: &str,
        _status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::list_by_status not used by curate tests")
    }

    async fn list_garbage(
        &self,
        _vault: &str,
        _before_ms: i64,
        _grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::list_garbage not used by curate tests")
    }

    async fn search_fts_for_forget(
        &self,
        _vault: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::search_fts_for_forget not used by curate tests")
    }

    async fn list_notes_by_agent(
        &self,
        _agent: &str,
        _vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::list_notes_by_agent not used by curate tests")
    }
}

// ── CurateFixture ─────────────────────────────────────────────────────────────

/// Bundle retourné par `test_curate_fixture` — garde les ressources vivantes
/// pour la durée du test (TempDir, Vault, index, client, curator, queue).
///
/// Le `TempDir` n'est PAS supprimé tant que le bundle est vivant — sécurité
/// pour éviter qu'un drop prématuré n'efface la base SQLite avant l'assertion.
///
/// `client`/`curator`/`queue` sont les dépendances que `handle_curate` attend
/// (moteur actif Apalis). Les assertions de tests opèrent directement sur
/// `vault` et `index` (mêmes `Arc`).
pub struct CurateFixture {
    pub vault: Arc<Vault>,
    pub index: Arc<SqliteIndex>,
    pub client: Arc<dyn InternalClient>,
    pub curator: Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>,
    pub queue: Arc<dyn QueueStore + Send + Sync>,
    pub _tmp: TempDir,
}

/// Crée un `SqliteQueueStore` in-memory avec schéma appliqué (backend JSON actif).
async fn test_store() -> SqliteQueueStore {
    let db = QueueDb::open_in_memory()
        .await
        .expect("db in-memory — invariant test fixture");
    apply_sqlite_pragmas(&db)
        .await
        .expect("pragmas — invariant test fixture");
    run_migrations(&db)
        .await
        .expect("migrations — invariant test fixture");
    SqliteQueueStore::new(db)
}

/// Construit une fixture curate active : vault, index, client mock, curator par
/// défaut et `SqliteQueueStore`.
///
/// Le `MockInternalClient` encapsule `vault` et `index` pour que `handle_curate`
/// puisse persister sans serveur HTTP. Les assertions de tests opèrent directement
/// sur `vault` et `index` (même `Arc`).
pub async fn test_curate_fixture() -> CurateFixture {
    let tmp = TempDir::new().expect("TempDir");
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(test_store().await);
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index: Arc<SqliteIndex> = vault.index().clone();

    let client: Arc<dyn InternalClient> =
        Arc::new(MockInternalClient::new(vault.clone(), index.clone()));
    let curator: Arc<dyn gradatum_curator::CuratorProcess + Send + Sync> =
        Arc::new(gradatum_curator::CuratorPipeline::new());

    CurateFixture {
        vault,
        index,
        client,
        curator,
        queue,
        _tmp: tmp,
    }
}

// ── Job builder + process ───────────────────────────────────────────────────────

/// Construit un `GradatumJob::Curate` (chemin vault_write : title + body présents).
///
/// Utilise le VRAI `gradatum_core::CurateSpec` (pas de miroir local) — tout nouveau
/// champ est automatiquement pris en compte, et l'ordre des champs n'a aucun effet
/// (sérialisation `serde_json`, indexée par nom).
///
/// `note_id` est un ULID frais + `expected_sha256 = None` → branche CREATE de
/// `handle_curate` (write inconditionnel via `write_note_with_id`).
fn make_curate_job(title: &str, body: &str) -> GradatumJob {
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

/// Traite un job `curate` via le moteur ACTIF `handle_curate` (title + body).
///
/// Remplace l'ancien couple `enqueue_curate_job` + `Dispatcher::run_once`. Le
/// curator par défaut décide Admitted/Pending selon l'heuristique (préfixe
/// `[DECISIONS]` → Admitted ; titre court sans préfixe → Pending). Dans les deux
/// cas les wikilinks `[[...]]` sont résolus et persistés (parité Admitted/Pending).
///
/// Retourne le `Result` de `handle_curate` pour que l'appelant assertionne l'issue.
pub async fn process_curate(
    fixture: &CurateFixture,
    title: &str,
    body: &str,
) -> Result<gradatum_core::JobOutput, HandlerError> {
    let job = make_curate_job(title, body);
    handle_curate(
        job,
        Data::new(Arc::clone(&fixture.client)),
        Data::new(Arc::clone(&fixture.curator)),
        Data::new(Arc::clone(&fixture.queue)),
        Data::new(MultiTenantCfg::default()),
    )
    .await
}

// ── Assertions helpers ──────────────────────────────────────────────────────────

/// Vérifie qu'une note source pointe vers `dst_id` dans `note_links` (vault `main`).
///
/// Wrapper sémantique autour de `idx.backlinks("main", dst_id)` — retourne `true` si
/// au moins un lien existe vers `dst_id`. Utilisé pour valider le
/// branchage B5 wikilinks post-curate.
pub async fn has_backlink_to(idx: &SqliteIndex, dst_id: &str) -> bool {
    let backs = idx.backlinks("main", dst_id).await.expect("backlinks main");
    !backs.is_empty()
}

/// Renvoie le nombre de backlinks vers `dst_id` (vault `main`).
pub async fn count_backlinks(idx: &SqliteIndex, dst_id: &str) -> usize {
    idx.backlinks("main", dst_id)
        .await
        .expect("backlinks main")
        .len()
}
