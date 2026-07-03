//! Test chaînage curate → embed_note.
//!
//! Vérifie que post `Dispatcher::run_once` sur un job `curate` admis,
//! un job `kind="embed_note"` est automatiquement enqueued avec un payload
//! contenant :
//! - `note_id` : ULID valide (non vide, parseable par `ulid::Ulid::from_string`)
//! - `body_text` : identique au body markdown soumis dans le job curate
//!
//! Complète les tests `e2e_write.rs`/`write_synthetic.rs` en validant le contenu
//! structurel du payload.
//!
//! ## Harness
//!
//! - Queue `SqliteQueue::in_memory` — isolation totale, aucun fichier sur disque.
//! - Vault dans `TempDir` — isolation entre exécutions.
//! - `Dispatcher` sans index ni embedder → `embed_note` sera skip si dispatché,
//!   mais ce test ne dispatche que le curate. L'embed_note reste pending.
//! - `CuratorPipeline::heuristic` — deterministe, pas de LLM.
//!
//! ## Payload embed_note
//!
//! Le dispatcher encode le payload avec `serde_json::to_vec` :
//! `{"note_id":"<ULID>","body_text":"<markdown body>"}`.
//! La queue stocke ces bytes opaques — on les désérialise ici pour inspecter.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_curator::CuratorPipeline;
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use gradatum_queue::{Queue as _, SqliteQueue};
use gradatum_server::api_v1::dto::VaultWriteRequest;
use gradatum_vault::Vault;
use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteIdDto, NoteReadDto,
};
use tempfile::TempDir;
use ulid::Ulid;

// ── ChainedJobsClient — mock minimal pour persist_curated (curate → write note) ─

struct ChainedJobsClient {
    vault: Arc<Vault>,
}

#[async_trait]
impl InternalClient for ChainedJobsClient {
    async fn persist_curated(
        &self,
        req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        use chrono::Utc;
        use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
        use gradatum_core::section::Section;
        use gradatum_core::status::NoteStatus;
        use smallvec::SmallVec;

        let section: Section =
            serde_json::from_str(&format!("\"{}\"", req.section)).unwrap_or(Section::Reference);
        let status: NoteStatus =
            serde_json::from_str(&format!("\"{}\"", req.status)).unwrap_or(NoteStatus::Live);

        let all_tags: SmallVec<[gradatum_core::tag::Tag; 4]> = req
            .tags
            .iter()
            .filter_map(|t| gradatum_core::tag::Tag::new(t.clone()).ok())
            .collect();

        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new(&req.tenant_id),
            locus: None,
            section,
            status,
            status_reason: None,
            status_changed: None,
            tags: all_tags,
            author: None,
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

        let note = self
            .vault
            .write_note_with_id(frontmatter, req.body.clone(), note_id)
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: e.to_string(),
            })?;

        Ok(PersistOkResponse {
            note_id: note.id.to_string(),
            status: "ok".to_string(),
        })
    }

    async fn persist_embedding(
        &self,
        _req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError> {
        unimplemented!()
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

    async fn delete_note(&self, _ulid: &str) -> Result<(), InternalClientError> {
        unimplemented!()
    }

    async fn get_note(&self, _ulid: &str) -> Result<NoteReadDto, InternalClientError> {
        unimplemented!()
    }

    async fn get_note_embedding(
        &self,
        _ulid: &str,
        _embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        unimplemented!()
    }

    async fn get_trust(&self, _ulid: &str) -> Result<f32, InternalClientError> {
        unimplemented!()
    }

    async fn title_lookup(
        &self,
        _tenant: &str,
        _title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        Ok(None) // no wikilinks in this test
    }

    async fn id_lookup(
        &self,
        _tenant: &str,
        _note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        Ok(None) // no wikilinks in this test
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

/// Body markdown soumis dans le job curate.
///
/// Suffisamment long pour passer le seuil d'admission heuristique (>20 chars)
/// et distinct du titre pour être identifiable dans le payload embed_note.
const CURATE_BODY: &str =
    "Ce test valide le chaînage automatique curate → embed_note en Phase 2.1.1.";

/// Titre préfixé `[DECISIONS]` : routé heuristiquement vers section `decisions`,
/// résultat déterministe (admitted), body_text préservé tel quel dans le payload.
const CURATE_TITLE: &str = "[DECISIONS] Chaînage embed_note — test Task 11";

/// Vérifie que `Dispatcher::run_once` sur un job `curate` admis enqueue automatiquement
/// un job `embed_note` dont le payload contient `note_id` (ULID valide) et
/// `body_text` (identique au body markdown soumis).
///
/// Ce test valide plus que `depth==1` : il inspecte la structure exacte du payload
/// du job chaîné via `queue.lease(&["embed_note"], ...)`.
#[tokio::test]
async fn curate_chains_embed_note_with_correct_payload() {
    // ── Infra ─────────────────────────────────────────────────────────────────
    let dir = TempDir::new().expect("tempdir chained_jobs");
    let vault_path = dir.path().join("vault");

    let vault = Arc::new(
        Vault::create(&vault_path, gradatum_core::scope::VaultId::new("main"))
            .await
            .expect("Vault::create chained_jobs"),
    );

    let queue = Arc::new(
        SqliteQueue::in_memory()
            .await
            .expect("SqliteQueue::in_memory chained_jobs"),
    );

    // ── Enqueue job curate ────────────────────────────────────────────────────
    let req = VaultWriteRequest {
        title: CURATE_TITLE.into(),
        body: CURATE_BODY.into(),
        author: None,
        tags: vec![],
        section_hint: None,
        tenant_id: "main".into(),
        expected_sha256: None,
        note_id: None,
        occurred_at: None,
    };
    let payload_bytes = bincode::serde::encode_to_vec(&req, bincode::config::standard())
        .expect("encode VaultWriteRequest bincode chained_jobs");

    queue
        .enqueue(gradatum_queue::NewJob {
            tenant_id: "main".into(),
            kind: "curate".into(),
            payload: payload_bytes,
            max_attempts: 3,
        })
        .await
        .expect("enqueue curate chained_jobs");

    // Sanity : 1 job pending avant dispatch.
    assert_eq!(
        queue.depth().await.expect("depth before chained_jobs"),
        1,
        "queue doit avoir 1 job pending avant run_once"
    );

    // ── Dispatcher::run_once ──────────────────────────────────────────────────
    // Embedder et index absents : les jobs embed_note seront skip si dispatchés,
    // mais on ne dispatche ici que le curate. L'embed_note restera pending.
    let curator = Arc::new(CuratorPipeline::heuristic());
    let dispatcher = Dispatcher::new(Arc::clone(&queue))
        .with_client(Arc::new(ChainedJobsClient {
            vault: Arc::clone(&vault),
        }) as Arc<dyn InternalClient>)
        .with_curator(curator)
        .with_audit(Arc::new(NoopAuditSink));

    let processed = dispatcher
        .run_once()
        .await
        .expect("Dispatcher::run_once chained_jobs");
    assert!(processed, "run_once doit retourner true (curate traité)");

    // ── Vérification chaînage ─────────────────────────────────────────────────
    // Le curate admis a chaîné automatiquement un job embed_note (Task 10).
    let depth_after = queue.depth().await.expect("depth after chained_jobs");
    assert_eq!(
        depth_after, 1,
        "queue doit contenir exactement 1 job (embed_note) après run_once"
    );

    // ── Inspection payload embed_note ─────────────────────────────────────────
    // Lease le job embed_note pour lire kind + payload sans le compléter.
    // On utilise une lease longue pour éviter expiration pendant l'inspection.
    let leased = queue
        .lease(&["embed_note"], Duration::from_secs(30))
        .await
        .expect("lease embed_note chained_jobs")
        .expect("embed_note doit être présent en queue après chaînage");

    // Assertion kind.
    assert_eq!(
        leased.kind, "embed_note",
        "le job chaîné doit avoir kind='embed_note', obtenu: {:?}",
        leased.kind
    );

    // Désérialise le payload JSON.
    // Format attendu : {"note_id":"<ULID>","body_text":"<markdown>"}
    let payload: serde_json::Value = serde_json::from_slice(&leased.payload)
        .expect("payload embed_note doit être du JSON valide");

    // Assertion note_id : présent + ULID valide.
    let note_id_str = payload["note_id"]
        .as_str()
        .expect("payload embed_note doit contenir 'note_id' string");
    assert!(!note_id_str.is_empty(), "note_id ne doit pas être vide");
    Ulid::from_string(note_id_str).expect("note_id doit être un ULID valide");

    // Assertion body_text : identique au body soumis dans le job curate.
    let body_text = payload["body_text"]
        .as_str()
        .expect("payload embed_note doit contenir 'body_text' string");
    assert_eq!(
        body_text, CURATE_BODY,
        "body_text dans le payload embed_note doit être identique au body curate soumis"
    );

    // Assertion tenant_id : propagé depuis le job curate parent.
    assert_eq!(
        leased.tenant_id, "main",
        "tenant_id du job embed_note doit être 'main'"
    );
}
