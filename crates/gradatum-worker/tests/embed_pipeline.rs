//! Tests d'intégration — handler `embed_note` du dispatcher worker.
//!
//! Vérifie :
//! - Cas succès : embedding calculé et persisté dans `note_embeddings`.
//! - Cas noop-skip : embedder absent → job traité sans insert (Ok silencieux).
//! - Cas dim-mismatch : le vecteur retourné ne correspond pas à la dimension
//!   déclarée → `insert_note_embedding` rejette, le job est marqué failed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
// VectorStore : insert_note_embedding, get_note_embedding (Étape 0.1 — méthodes *_inner pub(crate)).
use gradatum_core::VectorStore as _;
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use gradatum_embed::{EmbedBackend, EmbedError, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_queue::{NewJob, Queue, SqliteQueue};
use gradatum_vault::Vault;
use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteIdDto, NoteReadDto,
};
use tempfile::TempDir;
use ulid::Ulid;

// ── EmbedTestClient ───────────────────────────────────────────────────────────
//
// Minimal mock InternalClient for embed_pipeline tests.
// Implements persist_embedding → index.insert_note_embedding.
// Other methods are unimplemented (not called by embed_note path).

struct EmbedTestClient {
    index: Arc<SqliteIndex>,
}

impl EmbedTestClient {
    fn new(index: Arc<SqliteIndex>) -> Arc<Self> {
        Arc::new(Self { index })
    }
}

#[async_trait]
impl InternalClient for EmbedTestClient {
    async fn persist_curated(
        &self,
        _req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("EmbedTestClient::persist_curated not used in embed tests")
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
        unimplemented!()
    }

    async fn persist_distill(
        &self,
        _req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!()
    }

    async fn delete_note(&self, _vault_id: &str, _ulid: &str) -> Result<(), InternalClientError> {
        unimplemented!()
    }

    async fn get_note(
        &self,
        _vault_id: &str,
        _ulid: &str,
    ) -> Result<NoteReadDto, InternalClientError> {
        unimplemented!()
    }

    async fn get_note_status(
        &self,
        _vault_id: &str,
        _ulid: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!()
    }

    async fn get_note_embedding(
        &self,
        _vault_id: &str,
        _ulid: &str,
        _embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        unimplemented!()
    }

    async fn get_trust(&self, _vault_id: &str, _ulid: &str) -> Result<f32, InternalClientError> {
        unimplemented!()
    }

    async fn title_lookup(
        &self,
        _tenant: &str,
        _title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!()
    }

    async fn id_lookup(
        &self,
        _tenant: &str,
        _note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!()
    }

    async fn list_notes_by_locus(
        &self,
        _vault: &str,
        _prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!()
    }

    async fn list_by_status(
        &self,
        _vault: &str,
        _status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!()
    }

    async fn list_garbage(
        &self,
        _vault: &str,
        _before_ms: i64,
        _grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!()
    }

    async fn search_fts_for_forget(
        &self,
        _vault: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!()
    }

    async fn list_notes_by_agent(
        &self,
        _agent: &str,
        _vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!()
    }
}

// ── MockEmbedder ──────────────────────────────────────────────────────────────

/// Embedder de test — retourne des vecteurs contrôlables.
///
/// `return_dim`    : dimension déclarée par `dim()`.
/// `return_vec_len`: longueur réelle du vecteur retourné par `embed()`.
///   Si différente de `return_dim` → provoque un dim-mismatch dans `insert_note_embedding`.
struct MockEmbedder {
    id: &'static str,
    return_dim: u16,
    return_vec_len: usize,
    call_count: AtomicUsize,
}

impl MockEmbedder {
    /// Crée un embedder qui retourne des vecteurs de bonne dimension (succès).
    fn success(id: &'static str, dim: u16) -> Self {
        Self {
            id,
            return_dim: dim,
            return_vec_len: dim as usize,
            call_count: AtomicUsize::new(0),
        }
    }

    /// Crée un embedder dont le vecteur a une longueur incorrecte (dim-mismatch).
    fn dim_mismatch(id: &'static str, dim: u16, actual_len: usize) -> Self {
        Self {
            id,
            return_dim: dim,
            return_vec_len: actual_len,
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Embedder for MockEmbedder {
    fn embedder_id(&self) -> &str {
        self.id
    }

    fn dim(&self) -> u16 {
        self.return_dim
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(vec![0.1_f32; self.return_vec_len])
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Encode un payload JSON pour un job `embed_note`.
fn embed_note_payload(note_id: &str, body_text: &str) -> Vec<u8> {
    serde_json::json!({
        "note_id": note_id,
        "body_text": body_text,
    })
    .to_string()
    .into_bytes()
}

/// Construit une `Note` minimale valide — même pattern que `gradatum-index/tests/common.rs`.
///
/// Permet de satisfaire la FK `note_embeddings.note_id REFERENCES notes(id)` via `upsert_note`.
fn make_test_note(note_id: NoteId, body: &str) -> Note {
    let frontmatter = Frontmatter {
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
    let note_body = NoteBody {
        markdown: body.to_string(),
    };
    let content_hash = ContentHash::compute(&frontmatter, body);
    Note {
        id: note_id,
        frontmatter,
        body: note_body,
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Cas succès : l'embedder retourne un vecteur de bonne dimension.
/// Le job est traité et l'embedding est persisté dans `note_embeddings`.
#[tokio::test]
async fn embed_note_success_persists_embedding() {
    let dir = TempDir::new().unwrap();

    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let _vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );
    let index = Arc::new(SqliteIndex::open_in_memory().await.unwrap());

    // Insérer la note dans l'index pour satisfaire la FK note_embeddings.note_id
    let note_id = NoteId(Ulid::new());
    let note = make_test_note(note_id, "Contenu de la note à embedder.");
    index.upsert_note(&note).await.unwrap();

    // Enqueue job embed_note
    let payload = embed_note_payload(&note_id.to_string(), "Contenu de la note à embedder.");
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "embed_note".into(),
            payload,
            max_attempts: 3,
        })
        .await
        .unwrap();

    let embedder = Arc::new(MockEmbedder::success("mock-bge-small", 384));
    let dispatcher = Dispatcher::new(queue.clone())
        .with_client(EmbedTestClient::new(index.clone()))
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink))
        .with_embedder(embedder);

    let processed = dispatcher.run_once().await.unwrap();
    assert!(processed, "run_once doit signaler un job traité");

    // Vérifier l'embedding persisté
    let vec = index
        .get_note_embedding("main", &note_id, "mock-bge-small")
        .await
        .unwrap();
    assert!(
        vec.is_some(),
        "l'embedding doit être persisté dans note_embeddings"
    );
    let vec = vec.unwrap();
    assert_eq!(vec.len(), 384, "dim du vecteur doit être 384");
    assert!(
        (vec[0] - 0.1_f32).abs() < 1e-5,
        "valeur vecteur attendue 0.1, obtenu {}",
        vec[0]
    );

    // La queue doit être vide
    let again = dispatcher.run_once().await.unwrap();
    assert!(!again, "la queue doit être vide après traitement");
}

/// Cas noop-skip : le dispatcher n'a pas d'embedder configuré.
/// Le job est complété sans erreur ni insert.
#[tokio::test]
async fn embed_note_noop_skip_without_embedder() {
    let dir = TempDir::new().unwrap();

    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let _vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );

    let note_ulid = Ulid::new();
    let payload = embed_note_payload(&note_ulid.to_string(), "Corps quelconque.");
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "embed_note".into(),
            payload,
            max_attempts: 3,
        })
        .await
        .unwrap();

    // Dispatcher SANS embedder ni client — noop silencieux attendu (client absent → skip)
    let dispatcher = Dispatcher::new(queue.clone())
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher.run_once().await.unwrap();
    assert!(
        processed,
        "run_once doit signaler un job traité même en mode noop"
    );

    // La queue est vide — le job est bien marqué complete (pas failed)
    let again = dispatcher.run_once().await.unwrap();
    assert!(!again, "la queue doit être vide après le noop");
}

/// Cas dim-mismatch : l'embedder retourne un vecteur dont la longueur ne
/// correspond pas à `dim()` → `insert_note_embedding` rejette l'embedding.
/// `run_once` retourne `Ok(true)` mais le job est marqué failed dans la queue.
#[tokio::test]
async fn embed_note_dim_mismatch_job_fails() {
    let dir = TempDir::new().unwrap();

    let queue = Arc::new(
        SqliteQueue::new(&dir.path().join("queue.db"))
            .await
            .unwrap(),
    );
    let _vault = Arc::new(
        Vault::create(dir.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .unwrap(),
    );
    let index = Arc::new(SqliteIndex::open_in_memory().await.unwrap());

    // Insérer la note dans l'index (FK)
    let note_id = NoteId(Ulid::new());
    let note = make_test_note(note_id, "Corps quelconque.");
    index.upsert_note(&note).await.unwrap();

    let payload = embed_note_payload(&note_id.to_string(), "Corps quelconque.");
    queue
        .enqueue(NewJob {
            tenant_id: "main".into(),
            kind: "embed_note".into(),
            payload,
            max_attempts: 3,
        })
        .await
        .unwrap();

    // MockEmbedder : dim déclaré=384 mais vecteur réel=100 → mismatch
    let embedder = Arc::new(MockEmbedder::dim_mismatch("mock-mismatch", 384, 100));
    let dispatcher = Dispatcher::new(queue.clone())
        .with_client(EmbedTestClient::new(index.clone()))
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink))
        .with_embedder(embedder);

    // run_once doit retourner Ok(true) — l'erreur est loguée et le job marqué failed
    let processed = dispatcher.run_once().await.unwrap();
    assert!(
        processed,
        "run_once doit signaler un job traité (même en erreur)"
    );

    // Aucun embedding ne doit avoir été persisté
    let vec = index
        .get_note_embedding("main", &note_id, "mock-mismatch")
        .await
        .unwrap();
    assert!(
        vec.is_none(),
        "aucun embedding ne doit être persisté en cas de dim-mismatch"
    );
}
