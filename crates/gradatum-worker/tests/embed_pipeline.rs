//! Tests d'intégration — handler `embed_note` du dispatcher worker.
//!
//! Phase 2.1.1 Task 9 — vérifie :
//! - Cas succès : embedding calculé et persisté dans `note_embeddings`.
//! - Cas noop-skip : embedder absent → job traité sans insert (Ok silencieux).
//! - Cas dim-mismatch : le vecteur retourné ne correspond pas à la dimension
//!   déclarée → `insert_note_embedding` rejette, le job est marqué failed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
// VectorStore : insert_note_embedding, get_note_embedding (Étape 0.1 — méthodes *_inner pub(crate)).
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::VectorStore as _;
use gradatum_embed::{EmbedBackend, EmbedError, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_queue::{NewJob, Queue, SqliteQueue};
use gradatum_vault::Vault;
use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
use tempfile::TempDir;
use ulid::Ulid;

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
    let vault = Arc::new(
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
        .with_vault(vault)
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink))
        .with_index(index.clone())
        .with_embedder(embedder);

    let processed = dispatcher.run_once().await.unwrap();
    assert!(processed, "run_once doit signaler un job traité");

    // Vérifier l'embedding persisté
    let vec = index
        .get_note_embedding(&note_id, "mock-bge-small")
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
    let vault = Arc::new(
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

    // Dispatcher SANS embedder ni index — noop silencieux attendu
    let dispatcher = Dispatcher::new(queue.clone())
        .with_vault(vault)
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
    let vault = Arc::new(
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
        .with_vault(vault)
        .with_curator(Arc::new(gradatum_curator::CuratorPipeline::new()))
        .with_audit(Arc::new(NoopAuditSink))
        .with_index(index.clone())
        .with_embedder(embedder);

    // run_once doit retourner Ok(true) — l'erreur est loguée et le job marqué failed
    let processed = dispatcher.run_once().await.unwrap();
    assert!(
        processed,
        "run_once doit signaler un job traité (même en erreur)"
    );

    // Aucun embedding ne doit avoir été persisté
    let vec = index
        .get_note_embedding(&note_id, "mock-mismatch")
        .await
        .unwrap();
    assert!(
        vec.is_none(),
        "aucun embedding ne doit être persisté en cas de dim-mismatch"
    );
}
