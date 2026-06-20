//! Helpers tests partagés — worker B5 wikilinks + worker-flip MockInternalClient.
//!
//! Pattern TDD : `#[path = "helpers/mod.rs"] mod helpers;` au début de chaque
//! fichier test d'intégration touchant le worker B5.
//!
//! Fournit :
//! - `MockInternalClient` — implémente `InternalClient` en appelant `Vault`/`SqliteIndex`
//!   directement en local (pas de HTTP). Permet aux tests d'intégration de continuer
//!   à fonctionner sans un serveur HTTP.
//! - `test_dispatcher_with_index` — construit un `Dispatcher` complet avec le mock client.
//! - Helpers d'encodage et d'assertions B5 wikilinks.
//!
//! ## Architecture MockInternalClient
//!
//! - `persist_curated` → `vault.write_note_with_id(...)` + boucle sur `req.links` → `idx.upsert_link(...)`
//! - `persist_embedding` → `idx.insert_note_embedding(...)`
//! - `title_lookup` → `idx.title_lookup(...)`
//! - `get_note` → `vault.read_note(...)` + sérialisation vers `NoteReadDto`
//! - Autres méthodes : `unimplemented!()` (non utilisées par Dispatcher)

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use bincode::config::standard as bincode_std;
use gradatum_core::VectorStore as _;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use gradatum_index::SqliteIndex;
use gradatum_queue::{NewJob, Queue, SqliteQueue};
use gradatum_vault::Vault;
use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteIdDto, NoteReadDto,
};
use tempfile::TempDir;
use ulid::Ulid;

// ── MockInternalClient ────────────────────────────────────────────────────────

/// Mock `InternalClient` pour les tests d'intégration.
///
/// Délègue les mutations directement à `Vault` et `SqliteIndex` en local
/// (pas de HTTP). Permet au `Dispatcher` d'opérer sans serveur HTTP.
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
            vault_id: VaultId::new(&req.tenant_id),
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
            let _ = self.index.upsert_note_title(&note.id, &req.title).await;
        }

        // Process links — upsert each (src→dst) pair into note_links.
        // Non-fatal: a failed upsert_link is logged but does not fail the persist.
        for link in &req.links {
            if let Err(e) = self
                .index
                .upsert_link(&req.tenant_id, &link.src, &link.dst)
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
            .insert_note_embedding(&note_id, &req.embedder_id, req.dim, &req.vector)
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
        unimplemented!("MockInternalClient::persist_forget not used by Dispatcher tests")
    }

    async fn persist_distill(
        &self,
        _req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("MockInternalClient::persist_distill not used by Dispatcher tests")
    }

    async fn delete_note(&self, _ulid: &str) -> Result<(), InternalClientError> {
        unimplemented!("MockInternalClient::delete_note not used by Dispatcher tests")
    }

    // ── Reads ──

    async fn get_note(&self, ulid: &str) -> Result<NoteReadDto, InternalClientError> {
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

    async fn get_note_embedding(
        &self,
        _ulid: &str,
        _embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        unimplemented!("MockInternalClient::get_note_embedding not used by Dispatcher tests")
    }

    async fn get_trust(&self, _ulid: &str) -> Result<f32, InternalClientError> {
        unimplemented!("MockInternalClient::get_trust not used by Dispatcher tests")
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
        unimplemented!("MockInternalClient::list_notes_by_locus not used by Dispatcher tests")
    }

    async fn list_by_status(
        &self,
        _vault: &str,
        _status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::list_by_status not used by Dispatcher tests")
    }

    async fn list_garbage(
        &self,
        _vault: &str,
        _before_ms: i64,
        _grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::list_garbage not used by Dispatcher tests")
    }

    async fn search_fts_for_forget(
        &self,
        _vault: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::search_fts_for_forget not used by Dispatcher tests")
    }

    async fn list_notes_by_agent(
        &self,
        _agent: &str,
        _vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("MockInternalClient::list_notes_by_agent not used by Dispatcher tests")
    }
}

// ── DispatcherFixture ─────────────────────────────────────────────────────────

/// Bundle retourné par `test_dispatcher_with_index` — garde les ressources vivantes
/// pour la durée du test (TempDir, Vault, queue, dispatcher, index).
///
/// Le `TempDir` n'est PAS supprimé tant que le bundle est vivant — sécurité
/// pour éviter qu'un drop prématuré n'efface la base SQLite avant l'assertion.
pub struct DispatcherFixture {
    pub dispatcher: Dispatcher,
    pub queue: Arc<SqliteQueue>,
    pub vault: Arc<Vault>,
    pub index: Arc<SqliteIndex>,
    pub _tmp: TempDir,
}

/// Construit un dispatcher avec vault, queue, curator et mock client partagés.
///
/// Le `MockInternalClient` encapsule `vault` et `index` pour que le `Dispatcher`
/// puisse persister sans serveur HTTP. Les assertions de tests opèrent directement
/// sur `vault` et `index` (même Arc).
pub async fn test_dispatcher_with_index() -> DispatcherFixture {
    let tmp = TempDir::new().expect("TempDir");
    let queue = Arc::new(
        SqliteQueue::new(&tmp.path().join("queue.db"))
            .await
            .expect("SqliteQueue::new"),
    );
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index: Arc<SqliteIndex> = vault.index().clone();

    let mock_client = Arc::new(MockInternalClient::new(vault.clone(), index.clone()));

    let dispatcher = Dispatcher::new(queue.clone())
        .with_client(mock_client as Arc<dyn InternalClient>)
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink));

    DispatcherFixture {
        dispatcher,
        queue,
        vault,
        index,
        _tmp: tmp,
    }
}

// ── Payload helpers ───────────────────────────────────────────────────────────

/// Encode un payload `VaultWriteRequest` minimal (titre, body, section_hint).
///
/// `tenant_id="main"` codé en dur — cohérent avec le vault `VaultId::new("main")`.
fn encode_write_payload(title: &str, body: &str, section_hint: Option<&str>) -> Vec<u8> {
    // Miroir de `gradatum_dto::VaultWriteRequest` — ordre des champs INVARIANT (bincode positionnel).
    // Pos 6 = tenant_id, pos 7 = expected_sha256, pos 8 = note_id.
    // Ne pas modifier l'ordre sans aligner dispatch.rs + gradatum-dto.
    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    struct WriteReq {
        title: String,
        body: String,
        #[serde(default)]
        author: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        section_hint: Option<String>,
        #[serde(default = "default_main")]
        tenant_id: String,
        #[serde(default)]
        expected_sha256: Option<String>,
        #[serde(default)]
        note_id: Option<String>,
    }
    fn default_main() -> String {
        "main".into()
    }
    let req = WriteReq {
        title: title.into(),
        body: body.into(),
        author: None,
        tags: vec![],
        section_hint: section_hint.map(|s| s.to_string()),
        tenant_id: "main".into(),
        expected_sha256: None,
        note_id: None,
    };
    bincode::serde::encode_to_vec(&req, bincode_std()).expect("encode WriteReq bincode")
}

/// Enqueue un job `curate` pour titre + body donnés.
///
/// Le worker générera une décision `Admitted` (chemin par défaut sans heuristique
/// spéciale — le titre n'a pas le préfixe `[DECISIONS]/[BUG]/...` qui forcerait Pending).
pub async fn enqueue_curate_job(fixture: &DispatcherFixture, title: &str, body: &str) {
    let payload = encode_write_payload(title, body, None);
    fixture
        .queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .expect("enqueue curate");
}

/// Enqueue un job `curate` qui produira un `Pending` côté curator.
///
/// Mécanisme déclencheur Pending : le titre court (< 10 chars) sans préfixe explicite
/// + body court → confidence basse → CuratorPipeline défaut renvoie Pending.
///
/// Si le curator par défaut ne déclenche jamais Pending, le test peut être marqué
/// `#[ignore]` ou utiliser un curator stub. Vérifié empiriquement par le test
/// `curate_pending_outcome_also_upserts_wikilinks` — si le test échoue
/// avec un Admitted en lieu de Pending, ajuster le mécanisme déclencheur.
pub async fn enqueue_pending_curate_job(fixture: &DispatcherFixture, title: &str, body: &str) {
    let payload = encode_write_payload(title, body, None);
    fixture
        .queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload,
            max_attempts: 5,
        })
        .await
        .expect("enqueue pending curate");
}

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
