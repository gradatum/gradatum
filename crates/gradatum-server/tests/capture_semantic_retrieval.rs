//! E2E — critère d'acceptation F-246 : une ligne capturée est retrouvable par une
//! **question en langage naturel** dans une session ultérieure, **sans traitement
//! préalable** — donc via le bras sémantique, sur une requête qui ne reprend pas
//! les mots exacts de la ligne.
//!
//! # Pipeline exercé (bout en bout)
//!
//! 1. `POST /api/v1/capture` — deux lignes brutes (cible + distracteur sans rapport).
//! 2. Drain de la file : `handle_curate` écrit chaque note en section `snapshot`
//!    (statut `live`, section FORCÉE, corps intact) et enchaîne `Job::Embed`.
//! 3. Drain `handle_embed` — chaque note est VECTORISÉE.
//! 4. `POST /api/v1/vault_search` avec une question paraphrasée, `section=snapshot`.
//!    - `corpus_match_count == 0` prouve que le bras LEXICAL ne matche RIEN
//!      (la question ne reprend aucun mot de la ligne) ;
//!    - la note capturée remonte quand même → c'est le bras SÉMANTIQUE qui l'a
//!      retrouvée. C'est LE critère de la carte.
//!
//! # Embedder de test
//!
//! `TokenHashEmbedder` : vecteur déterministe **sensible au contenu** (trigrammes +
//! 4-grammes de caractères, hachés en sac-de-mots L2-normalisé). Deux textes qui
//! partagent des sous-mots (ex. `surveillance`/`surveille`, `anormale`/`normal`)
//! obtiennent une similarité cosine positive — sans partager un seul mot entier.
//! C'est ce qui permet de prouver la récupération sémantique hors-ligne.

use std::sync::Arc;

use apalis::prelude::Data;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Router, middleware};
use chrono::Utc;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::VectorStore;
use gradatum_core::author::AuthorRef;
use gradatum_core::error::GradatumError;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::index::{AnchorSrc, TemporalEntry};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_core::{GradatumJob, Job, JobFilter, QueueStore};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_dto::{
    EmbeddingOkResponse, EventLogResponse, PersistCuratedRequest, PersistDistillRequest,
    PersistEmbeddingRequest, PersistForgetRequest, PersistOkResponse,
};
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::state::AppState;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::{MultiTenantCfg, handle_curate, handle_embed};
use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteReadDto,
};
use http_body_util::BodyExt;
use smallvec::SmallVec;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

// ── Constantes du scénario ─────────────────────────────────────────────────────

/// Ligne cible capturée — le SUJET que la question sémantique doit retrouver.
const TARGET_LINE: &str =
    "le démon de surveillance a détecté une latence anormale sur la file de traitement";

/// Ligne distracteur — capturée aussi, mais SANS rapport avec la question.
const DISTRACTOR_LINE: &str = "la météo annonce un orage violent pour la région ce soir";

/// Question en langage naturel — ne reprend AUCUN mot de `TARGET_LINE`.
/// Les mots de la question (`processus`, `surveille`, `délais`, `inhabituels`,
/// `queue`, `exécution`, `normal`) sont sémantiquement proches des mots de la
/// ligne (`démon`, `surveillance`, `latence`, `anormale`, `file`, `traitement`)
/// via des sous-mots partagés (`surveill…`, `…normale`/`normal`).
const SEMANTIC_QUERY: &str =
    "un processus qui surveille les délais inhabituels dans la queue d'exécution est-il normal ?";

const TEST_ACL: &str = r#"
[[consumer]]
identity = "capture-tester"
read_patterns  = ["**"]
write_patterns = ["main/snapshot"]
"#;

/// Mots-outils français exclus de l'assertion d'absence de mot commun : ce sont
/// des connecteurs grammaticalux, pas du contenu. L'assertion porte sur les mots
/// de CONTENU (le sens), pas sur les articles/prépositions.
const FR_STOPWORDS: &[&str] = &[
    "a", "au", "aux", "ce", "ces", "cet", "d", "dans", "de", "des", "du", "elle", "en", "est",
    "et", "il", "la", "le", "les", "on", "ou", "par", "pour", "que", "qui", "qu", "sur", "un",
    "une",
];

// ─────────────────────────────────────────────────────────────────────────────
// Embedder déterministe sensible au contenu
// ─────────────────────────────────────────────────────────────────────────────

/// Sac-de-mots de n-grammes de caractères (trigrammes + 4-grammes), L2-normalisé.
///
/// Deux textes partageant des sous-mots (racines morphologiques) obtiennent une
/// similarité cosine positive même sans mot entier commun — comportement
/// « sémantique » reproduit hors-ligne, déterministe.
struct TokenHashEmbedder {
    dim: u16,
}

impl TokenHashEmbedder {
    fn new(dim: u16) -> Self {
        Self { dim }
    }

    fn embed_text(&self, text: &str) -> Vec<f32> {
        let flat: String = normalized_words(text).join("");
        let chars: Vec<char> = flat.chars().collect();
        let mut v = vec![0.0f32; self.dim as usize];
        // Trigrammes (n=3, poids 1.0) + 4-grammes (n=4, poids 3.0) : les 4-grammes
        // sont nettement plus discriminants (racines morphologiques partagées type
        // `surv…`, `norm`), et la grande dimension (4096) évite que les collisions
        // de hachage ne fabriquent du bruit commun français.
        for n in [3usize, 4] {
            let weight = if n == 4 { 3.0f32 } else { 1.0f32 };
            for w in chars.windows(n) {
                let key: String = w.iter().collect();
                let idx = (fnv1a(key.as_bytes()) % u64::from(self.dim)) as usize;
                v[idx] += weight;
            }
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

#[async_trait]
impl Embedder for TokenHashEmbedder {
    fn embedder_id(&self) -> &str {
        "token-hash-test"
    }
    fn dim(&self) -> u16 {
        self.dim
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Http
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self.embed_text(text))
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| self.embed_text(t)).collect())
    }
}

/// FNV-1a 64-bit — hash stable, déterministe, sans allocation.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Diacritiques du français → forme nue (parité avec le tokeniseur FTS unicode61).
fn strip_diacritics(c: char) -> char {
    match c {
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'à' | 'â' | 'ä' => 'a',
        'î' | 'ï' => 'i',
        'ô' | 'ö' => 'o',
        'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        other => other,
    }
}

/// Mots normalisés : minuscules, diacritiques ôtées, suite alphanumérique.
fn normalized_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        let c = strip_diacritics(c);
        if c.is_alphanumeric() {
            cur.push(c);
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Mots de contenu (mots-outils exclus) — pour l'assertion d'absence de mot commun.
fn content_words(text: &str) -> Vec<String> {
    normalized_words(text)
        .into_iter()
        .filter(|w| !FR_STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Similarité cosine entre deux vecteurs.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    dot
}

// ─────────────────────────────────────────────────────────────────────────────
// Client interne minimal — 3 méthodes réelles, le reste en stubs non atteints
// ─────────────────────────────────────────────────────────────────────────────

/// `InternalClient` minimal pour le test E2E : `persist_curated`, `get_note`,
/// `persist_embedding` sont réels (Vault + SqliteIndex réels) ; les autres
/// méthodes ne sont jamais appelées par `handle_curate`/`handle_embed` sur le
/// chemin CREATE sans wikilinks — stubs `unimplemented!()`.
struct CaptureTestClient {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
}

impl CaptureTestClient {
    fn new(vault: Arc<Vault>, index: Arc<SqliteIndex>) -> Self {
        Self { vault, index }
    }
}

#[async_trait]
impl InternalClient for CaptureTestClient {
    async fn persist_curated(
        &self,
        req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        let note_id = Ulid::from_string(&req.note_id).map(NoteId).map_err(|e| {
            InternalClientError::ServerError {
                status: 400,
                body: format!("{e}"),
            }
        })?;
        let section = parse_section(&req.section)?;
        let status = parse_status(&req.status)?;
        let author_ref = req
            .author
            .as_deref()
            .map(parse_author)
            .transpose()
            .map_err(|e| InternalClientError::ServerError {
                status: 400,
                body: format!("invalid author: {e}"),
            })?;
        let tags = parse_tags(&req.tags)?;

        let frontmatter = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("main"),
            locus: None,
            section,
            status,
            status_reason: None,
            status_changed: None,
            tags,
            author: author_ref,
            created: Utc::now(),
            updated: None,
            extra: ExtraFields::empty(),
            provenance: req.provenance.clone(),
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };

        let written = self
            .vault
            .write_note_with_id(frontmatter, req.body.clone(), note_id)
            .await
            .map_err(|e| vault_err_to_client(e, &req.note_id))?;

        let _ = self
            .index
            .upsert_note_title(
                written.frontmatter.vault_id.as_str(),
                &written.id,
                &req.title,
            )
            .await;

        if let Some(temporal) = &req.temporal {
            let anchor_src = match temporal.anchor_src.as_str() {
                "occurred_at" | "OccurredAt" => AnchorSrc::OccurredAt,
                "event-date" | "EventDate" => AnchorSrc::EventDate,
                "valid_from" | "ValidFrom" => AnchorSrc::ValidFrom,
                _ => AnchorSrc::Created,
            };
            let entry = TemporalEntry {
                note_id: req.note_id.clone(),
                vault_id: "main".to_string(),
                anchor_ms: temporal.anchor_ms,
                anchor_src,
                doc_kind: temporal.doc_kind.clone(),
                valid_until_ms: temporal.valid_until_ms,
            };
            let _ = self.index.write_temporal_entry(&entry).await;
        }

        for link in &req.links {
            let _ = self.index.upsert_link("main", &link.src, &link.dst).await;
        }

        if let Some(trust) = req.trust {
            let _ = self
                .index
                .set_note_trust(written.frontmatter.vault_id.as_str(), &written.id, trust)
                .await;
        }

        Ok(PersistOkResponse {
            note_id: req.note_id.clone(),
            status: "ok".to_string(),
        })
    }

    async fn get_note(
        &self,
        _vault_id: &str,
        ulid: &str,
    ) -> Result<NoteReadDto, InternalClientError> {
        let note_id =
            Ulid::from_string(ulid)
                .map(NoteId)
                .map_err(|e| InternalClientError::ServerError {
                    status: 400,
                    body: format!("{e}"),
                })?;
        let note = self
            .vault
            .read_note(note_id)
            .await
            .map_err(|e| vault_err_to_client(e, ulid))?;
        Ok(NoteReadDto {
            note_id: ulid.to_string(),
            sha256_hex: note.content_hash.hex(),
            body: note.body.markdown,
            section: section_to_str(note.frontmatter.section),
            status: status_to_str(note.frontmatter.status),
            tags: note
                .frontmatter
                .tags
                .iter()
                .map(|t| t.as_str().to_string())
                .collect(),
            forgotten: note.frontmatter.forgotten.unwrap_or(false),
            processed: note
                .frontmatter
                .extra
                .get("processed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }

    async fn persist_embedding(
        &self,
        req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError> {
        let note_id = Ulid::from_string(&req.note_id).map(NoteId).map_err(|e| {
            InternalClientError::ServerError {
                status: 400,
                body: format!("{e}"),
            }
        })?;
        let dim = req.vector.len();
        self.index
            .insert_note_embedding("main", &note_id, &req.embedder_id, req.dim, &req.vector)
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: format!("{e}"),
            })?;
        Ok(EmbeddingOkResponse {
            note_id: req.note_id.clone(),
            embedder_id: req.embedder_id.clone(),
            dim,
        })
    }

    async fn persist_forget(
        &self,
        _req: &PersistForgetRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("capture e2e: persist_forget non atteint")
    }
    async fn persist_distill(
        &self,
        _req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("capture e2e: persist_distill non atteint")
    }
    async fn delete_note(&self, _vault_id: &str, _ulid: &str) -> Result<(), InternalClientError> {
        unimplemented!("capture e2e: delete_note non atteint")
    }
    async fn get_note_status(
        &self,
        _vault_id: &str,
        _ulid: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!("capture e2e: get_note_status non atteint")
    }
    async fn get_note_embedding(
        &self,
        _vault_id: &str,
        _ulid: &str,
        _embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        unimplemented!("capture e2e: get_note_embedding non atteint")
    }
    async fn get_trust(&self, _vault_id: &str, _ulid: &str) -> Result<f32, InternalClientError> {
        unimplemented!("capture e2e: get_trust non atteint")
    }
    async fn title_lookup(
        &self,
        _tenant: &str,
        _title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!("capture e2e: title_lookup non atteint")
    }
    async fn id_lookup(
        &self,
        _tenant: &str,
        _note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!("capture e2e: id_lookup non atteint")
    }
    async fn list_notes_by_locus(
        &self,
        _vault: &str,
        _prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("capture e2e: list_notes_by_locus non atteint")
    }
    async fn list_by_status(
        &self,
        _vault: &str,
        _status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("capture e2e: list_by_status non atteint")
    }
    async fn list_garbage(
        &self,
        _vault: &str,
        _before_ms: i64,
        _grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("capture e2e: list_garbage non atteint")
    }
    async fn search_fts_for_forget(
        &self,
        _vault: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("capture e2e: search_fts_for_forget non atteint")
    }
    async fn list_notes_by_agent(
        &self,
        _agent: &str,
        _vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("capture e2e: list_notes_by_agent non atteint")
    }
}

use gradatum_worker::internal_client::NoteIdDto;

// ── Helpers de parse (parité avec test_internal_client.rs) ────────────────────

fn parse_section(s: &str) -> Result<Section, InternalClientError> {
    Section::from_canonical_str(s).ok_or_else(|| InternalClientError::ServerError {
        status: 400,
        body: format!("section invalide : {s:?}"),
    })
}

fn parse_status(s: &str) -> Result<NoteStatus, InternalClientError> {
    match s {
        "draft" => Ok(NoteStatus::Draft),
        "live" | "Live" => Ok(NoteStatus::Live),
        "pending-review" | "PendingReview" | "Pending" => Ok(NoteStatus::PendingReview),
        "staging" | "Staging" => Ok(NoteStatus::Staging),
        "garbage" | "Garbage" => Ok(NoteStatus::Garbage),
        "archived" | "deprecated" | "Deprecated" => Ok(NoteStatus::Deprecated),
        _ => Err(InternalClientError::ServerError {
            status: 400,
            body: format!("statut invalide : {s:?}"),
        }),
    }
}

fn parse_author(s: &str) -> Result<AuthorRef, GradatumError> {
    if s.trim().is_empty() {
        return Err(GradatumError::InvalidInput(
            "empty author — no identity resolved (R2)".to_string(),
        ));
    }
    Ok(AuthorRef {
        kind: gradatum_core::author::AuthorKind::MainAgent,
        id: s.to_string(),
        display_name: None,
    })
}

fn parse_tags(tags: &[String]) -> Result<SmallVec<[Tag; 4]>, InternalClientError> {
    tags.iter()
        .map(|t| {
            Tag::new(t.clone()).map_err(|e| InternalClientError::ServerError {
                status: 400,
                body: format!("tag invalide : {e}"),
            })
        })
        .collect()
}

fn section_to_str(section: Section) -> String {
    section.as_str().to_string()
}

fn status_to_str(status: NoteStatus) -> String {
    match status {
        NoteStatus::Live => "live".to_string(),
        NoteStatus::PendingReview => "pending-review".to_string(),
        NoteStatus::Staging => "staging".to_string(),
        NoteStatus::Garbage => "garbage".to_string(),
        NoteStatus::Draft => "draft".to_string(),
        NoteStatus::Deprecated => "deprecated".to_string(),
    }
}

fn vault_err_to_client(e: gradatum_vault::VaultError, note_id: &str) -> InternalClientError {
    match e {
        gradatum_vault::VaultError::Core(inner) => {
            let msg = format!("{inner}");
            if msg.contains("not found") || msg.contains("introuvable") {
                InternalClientError::NotFound {
                    ulid: note_id.to_string(),
                }
            } else {
                InternalClientError::ServerError {
                    status: 500,
                    body: msg,
                }
            }
        }
        other => InternalClientError::ServerError {
            status: 500,
            body: format!("{other}"),
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Harness E2E
// ─────────────────────────────────────────────────────────────────────────────

struct E2EHarness {
    app: Router,
    state: AppState,
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    queue: Arc<SqliteQueueStore>,
    client: Arc<dyn InternalClient>,
    embedder: Arc<TokenHashEmbedder>,
    _tmp: TempDir,
}

async fn build_e2e() -> E2EHarness {
    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL");

    let tmp = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );
    let index = vault.index().clone();

    let db = QueueDb::open_in_memory().await.expect("pool in-memory");
    apply_sqlite_pragmas(&db).await.expect("pragmas");
    run_migrations(&db).await.expect("migrations");
    let queue = Arc::new(SqliteQueueStore::new(db.clone()));

    let embedder = Arc::new(TokenHashEmbedder::new(4096));

    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_embedder(Arc::clone(&embedder) as Arc<dyn Embedder>)
        .with_job_store(Arc::clone(&queue) as Arc<dyn QueueStore>, db);
    state.search = Arc::clone(&index) as Arc<dyn gradatum_core::index::Index>;

    let client: Arc<dyn InternalClient> = Arc::new(CaptureTestClient::new(
        Arc::clone(&vault),
        Arc::clone(&index),
    ));

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    E2EHarness {
        app,
        state,
        vault,
        index,
        queue,
        client,
        embedder,
        _tmp: tmp,
    }
}

/// POST JSON authentifié → (status, body).
async fn post_json(app: &Router, uri: &str, token: &str, body: serde_json::Value) -> StatusCode {
    let req = Request::builder()
        .uri(uri)
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    resp.status()
}

/// Drain de la file : exécute les jobs `Curate` (→ notes + enqueue `Embed`), puis
/// les jobs `Embed` (→ vectorisation). Reproduit la cascade du monitor en séquence.
/// `expected` documente le nombre de lignes capturées (une job Curate + un job
/// Embed par ligne).
async fn drain_curate_then_embed(h: &E2EHarness, expected: usize) {
    let curator: Arc<dyn gradatum_curator::CuratorProcess + Send + Sync> =
        Arc::new(gradatum_curator::CuratorPipeline::new());
    let mt = Data::new(MultiTenantCfg::default());

    let curates: Vec<gradatum_core::JobRecord> = h
        .queue
        .list(JobFilter::default())
        .await
        .expect("list curates")
        .into_iter()
        .filter(|j| matches!(j.spec.kind, Job::Curate(_)))
        .collect();
    assert_eq!(
        curates.len(),
        expected,
        "jobs curate enqueués par le endpoint"
    );
    for rec in curates {
        let job = GradatumJob {
            priority: rec.spec.priority.as_u8(),
            record: rec,
        };
        handle_curate(
            job,
            Data::new(Arc::clone(&h.client)),
            Data::new(Arc::clone(&curator)),
            Data::new(Arc::clone(&h.queue) as Arc<dyn QueueStore + Send + Sync>),
            mt,
        )
        .await
        .expect("handle_curate doit réussir");
    }

    let embeds: Vec<gradatum_core::JobRecord> = h
        .queue
        .list(JobFilter::default())
        .await
        .expect("list embeds")
        .into_iter()
        .filter(|j| matches!(j.spec.kind, Job::Embed(_)))
        .collect();
    assert_eq!(
        embeds.len(),
        expected,
        "le chaînage curate→embed a enqueu les jobs"
    );
    for rec in embeds {
        let job = GradatumJob {
            priority: rec.spec.priority.as_u8(),
            record: rec,
        };
        handle_embed(
            job,
            Data::new(Arc::clone(&h.client)),
            Data::new(Arc::clone(&h.embedder) as Arc<dyn Embedder>),
            mt,
        )
        .await
        .expect("handle_embed doit réussir");
    }
}

/// Retrouve l'ULID de la note `snapshot` dont le corps contient `needle`.
async fn snapshot_note_id_by_body(h: &E2EHarness, needle: &str) -> Ulid {
    let ids = h
        .index
        .list_by_status(&VaultId::new("main"), NoteStatus::Live)
        .await
        .expect("list_by_status");
    for id in ids {
        if let Ok(note) = h.vault.read_note(id).await
            && note.body.markdown.contains(needle)
        {
            return id.0;
        }
    }
    panic!("aucune note snapshot ne contient {needle:?}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Calibration de l'embedder — documente la séparation sémantique du couple
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn token_hash_embedder_separates_target_from_distractor() {
    let emb = TokenHashEmbedder::new(4096);
    let q = emb.embed_text(SEMANTIC_QUERY);
    let target = emb.embed_text(TARGET_LINE);
    let distractor = emb.embed_text(DISTRACTOR_LINE);

    let cos_target = cosine(&q, &target);
    let cos_distractor = cosine(&q, &distractor);
    assert!(
        cos_target > 0.03,
        "la question doit être sémantiquement proche de la ligne cible (cos={cos_target:.3})"
    );
    assert!(
        cos_target > cos_distractor * 3.0,
        "la cible doit dominer le distracteur (target={cos_target:.3}, distractor={cos_distractor:.3})"
    );

    // Preuve que la requête ne reprend aucun mot de CONTENU de la ligne.
    let target_content = content_words(TARGET_LINE);
    let query_content = content_words(SEMANTIC_QUERY);
    let common: Vec<&String> = query_content
        .iter()
        .filter(|w| target_content.contains(w))
        .collect();
    assert!(
        common.is_empty(),
        "la question reprend des mots de contenu de la ligne : {common:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// LE test du critère d'acceptation
// ─────────────────────────────────────────────────────────────────────────────

/// Une ligne capturée est retrouvable par une question en langage naturel dans
/// une session ultérieure, sans traitement préalable — via le bras sémantique.
#[tokio::test]
async fn captured_line_found_by_natural_language_question() {
    let h = build_e2e().await;
    let token = h
        .state
        .jwt
        .sign(
            "capture-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt");

    // 1. Capture — l'appelant ne fournit QUE les lignes brutes.
    let status = post_json(
        &h.app,
        "/api/v1/capture",
        &token,
        serde_json::json!([TARGET_LINE, DISTRACTOR_LINE]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "capture acceptée");

    // 2. Le worker traite curate puis embed (section snapshot forcée, statut live,
    //    vectorisation).
    drain_curate_then_embed(&h, 2).await;

    let target_id = snapshot_note_id_by_body(&h, "surveillance").await;
    let distractor_id = snapshot_note_id_by_body(&h, "météo").await;
    let target_path = format!("snapshot/{target_id}");
    let distractor_path = format!("snapshot/{distractor_id}");

    // 3. Recherche sémantique — question en langage naturel, section snapshot,
    //    comptage lexical inclus pour prouver l'absence de match BM25.
    let body = serde_json::json!({
        "query": SEMANTIC_QUERY,
        "section": "snapshot",
        "include_corpus_count": true,
    });
    let req = Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "recherche OK");
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");

    // 4a. Le bras LEXICAL ne matche rien : la question ne reprend aucun mot de la
    //     ligne. Le comptage de corpus (FTS5/BM25, section snapshot) est à 0.
    assert_eq!(
        json["corpus_match_count"].as_u64(),
        Some(0),
        "corpus_match_count doit être 0 — la question est une paraphrase, pas un copier-coller"
    );

    // 4b. La note capturée remonte quand même → c'est le bras SÉMANTIQUE qui l'a
    //     retrouvée. C'est LE critère de la carte.
    let items = json["items"].as_array().expect("items");
    let paths: Vec<String> = items
        .iter()
        .map(|i| i["path"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        paths.contains(&target_path),
        "la ligne capturée doit être retrouvée par la question — paths: {paths:?}"
    );

    // 4c. Elle est classée AU-DESSUS du distracteur (pertinence, pas exhaustivité).
    let target_rank = paths.iter().position(|p| p == &target_path).expect("cible");
    let distractor_rank = paths.iter().position(|p| p == &distractor_path);
    assert!(
        distractor_rank.is_none_or(|r| target_rank < r),
        "la cible doit être classée avant le distracteur (target_rank={target_rank}, \
         distractor_rank={distractor_rank:?}, paths={paths:?})"
    );

    // 4d. Statut de la note — l'écriture a forcé `live` (le `path` porte déjà la
    //     section `snapshot/…`).
    let hit = items
        .iter()
        .find(|i| i["path"].as_str() == Some(&target_path))
        .expect("hit cible");
    assert_eq!(hit["status"].as_str(), Some("live"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests de contrat du endpoint capture (miroir event_log)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn capture_rejects_over_batch_size_with_413() {
    let h = build_e2e().await;
    let token = h
        .state
        .jwt
        .sign(
            "capture-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt");
    // 1001 lignes > MAX_BATCH_SIZE (1000).
    let lines: Vec<String> = (0..1001).map(|_| "ligne".to_string()).collect();
    let status = post_json(
        &h.app,
        "/api/v1/capture",
        &token,
        serde_json::to_value(lines).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "batch > 1000 → 413");
}

#[tokio::test]
async fn capture_rejects_overlong_line_with_422() {
    let h = build_e2e().await;
    let token = h
        .state
        .jwt
        .sign(
            "capture-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt");
    // 1025 chars > MAX_FIELD_LEN (1024).
    let status = post_json(
        &h.app,
        "/api/v1/capture",
        &token,
        serde_json::json!(["x".repeat(1025)]),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "ligne > 1024 → 422"
    );
}

#[tokio::test]
async fn capture_unauthenticated_returns_401() {
    let h = build_e2e().await;
    let status = post_json(&h.app, "/api/v1/capture", "", serde_json::json!(["ligne"])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "non authentifié → 401");
}

#[tokio::test]
async fn capture_response_reports_accepted_count() {
    let h = build_e2e().await;
    let token = h
        .state
        .jwt
        .sign(
            "capture-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt");
    let req = Request::builder()
        .uri("/api/v1/capture")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!(["ligne a", "ligne b"])).unwrap(),
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let parsed: EventLogResponse = serde_json::from_slice(&bytes).expect("parse");
    assert_eq!(parsed.accepted_count, 2);
    assert_eq!(parsed.status, "accepted");
}

/// Garantie structurelle : la section de la note est FORCÉE à `snapshot` par le
/// hint fort du curateur — le corps n'est ni reclassé, ni recomposé.
#[tokio::test]
async fn captured_note_is_written_in_snapshot_with_intact_body() {
    let h = build_e2e().await;
    let token = h
        .state
        .jwt
        .sign(
            "capture-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("jwt");
    let _ = post_json(
        &h.app,
        "/api/v1/capture",
        &token,
        serde_json::json!([TARGET_LINE]),
    )
    .await;
    drain_curate_then_embed(&h, 1).await;

    let id = snapshot_note_id_by_body(&h, "surveillance").await;
    let note = h.vault.read_note(NoteId(id)).await.expect("note écrite");
    // Section forcée — pas une suggestion du curateur.
    assert_eq!(note.frontmatter.section, Section::Snapshot);
    // Corps intact — aucune recomposition à la capture.
    assert_eq!(note.body.markdown, TARGET_LINE);
    // Statut vectorisable (live).
    assert_eq!(note.frontmatter.status, NoteStatus::Live);

    // Titre mécanique composé côté serveur (jamais fourni par l'appelant) — stocké
    // dans la colonne `notes.title` de l'index via `upsert_note_title`.
    let (title, _) = h
        .index
        .get_titles_sections("main", &[id.to_string()])
        .await
        .expect("get_titles_sections")
        .get(&id.to_string())
        .cloned()
        .expect("titre présent dans l'index");
    let title = title.expect("titre non vide");
    assert!(
        title.starts_with("snapshot "),
        "le titre doit être mécanique (snapshot <horodatage> <ulid>) — trouvé : {title}"
    );
    assert!(
        title.contains(&id.to_string()),
        "le titre porte le discriminant ULID — trouvé : {title}"
    );
}
