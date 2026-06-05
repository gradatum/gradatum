//! État applicatif partagé — Arc-able, clonable entre les handlers.
//!
//! T1 livre le skeleton minimal (uptime + version).
//! T11 ajoute `AppMetrics` (canal latéral métriques :19091).
//! T8 ajoute `acl`, `revocation` (+ stubs `vault`/`search` en attente T2/T3 P2.0c).
//! T9 ajoute `jwt: Arc<JwtService>` — vraie vérification bearer.
//! T1 P2.0c : `NoopQueue` supprimé de stubs.rs — `with_queue_path` injecte `SqliteQueue`.
//! T2 P2.0c : `VaultRegistryStub` supprimé de stubs.rs — `with_vault_path` injecte `Vault`.
//! T7 P2.0c : `audit: Arc<dyn AuditSink>` injecté via `with_audit_dir` (JsonlFileSink prod).
//! T8 P2.1.1 : `embedder: Arc<dyn Embedder>` injecté via `with_embedder` (HttpEmbedder prod, Noop dev).
//!
//! # Stubs P2.0c
//!
//! `AppState::search` reste un Arc sur un stub (T3 P2.0c).
//! `AppState::vault` est désormais `Arc<dyn gradatum_vault::Registry>` — câblé via
//! `with_vault_path` en production et `PlaceholderRegistry` dans les constructeurs sync.
//!
//! `AppState::jwt` (JwtService) et `AppState::acl` (AclEngine) sont les vraies
//! implémentations — initialisées avec des valeurs par défaut safe pour les tests.
//!
//! `AppState::audit` est un `NoopAuditSink` par défaut — câblé via `with_audit_dir`
//! en production pour produire des fichiers JSONL rotatifs.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gradatum_embed::{Embedder, Noop as NoopEmbedder};

use async_trait::async_trait;
use gradatum_acl_auth::{ApiKeyStore, SqliteApiKeyStore};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_auth::revocation::{InMemoryRevocationStore, RevocationStore};
use gradatum_core::audit::http::AuditSink;
use gradatum_core::error::GradatumError;
use gradatum_core::QueueStore;
use gradatum_vault::Registry;
use sqlx::SqlitePool;

use gradatum_queue::{JobId, JobInfo, LeasedJob, NewJob, Queue, QueueError};

use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;

use crate::event_log_store::EventLogStore;
use crate::metrics::AppMetrics;

// ── NoopAuditSink (state.rs) ──────────────────────────────────────────────────

/// Sink d'audit no-op pour les tests et constructeurs sync.
///
/// Remplacé par `JsonlFileSink` en production via `AppState::with_audit_dir`.
/// Distinct du `NoopAuditSink` de `gradatum-worker` — chaque crate conserve le sien
/// pour éviter la dépendance croisée (pattern DTOs locaux T5).
struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    /// Ne fait rien — les événements sont silencieusement ignorés.
    async fn record(
        &self,
        _event: gradatum_core::audit::http::HttpAuditEvent,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

// ── Placeholders internes ──────────────────────────────────────────────────────

/// Initialise un `SqliteIndex` en mémoire pour les constructeurs sync.
///
/// Utilisé par `with_jwt` et `with_jwt_and_acl` pour remplir le champ `search`
/// avant injection via `with_search_path`.
///
/// Utilise un thread dédié avec son propre runtime tokio pour contourner la
/// restriction de `block_on` dans un runtime `current_thread` (utilisé par les
/// tests `#[tokio::test]`). Le thread est joint immédiatement — pas de fuite.
///
/// Retourne `Arc<dyn Index>` — le type concret `SqliteIndex` est effacé immédiatement
/// pour correspondre au champ `AppState.search : Arc<dyn Index>` (Étape 0.2a).
///
/// # Panics
///
/// Panique uniquement si l'init SQLite in-memory échoue — impossible en pratique.
fn placeholder_search() -> Arc<dyn Index> {
    let idx = std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mini-runtime placeholder_search — invariant build")
            .block_on(SqliteIndex::open_in_memory())
            .expect("SqliteIndex::open_in_memory() placeholder — invariant init search")
    })
    .join()
    .expect("thread join placeholder_search — invariant thread");
    Arc::new(idx) as Arc<dyn Index>
}

/// Queue placeholder pour les constructeurs sync (`with_jwt`, `with_jwt_and_acl`).
///
/// Remplacé immédiatement par `SqliteQueue` via `with_queue_path` en production
/// ou par `SqliteQueue::in_memory()` via `with_queue` dans les tests.
///
/// Ce type est privé à `state.rs` — non exposé dans l'API publique du crate.
/// Il permet aux constructeurs sync d'initialiser `AppState` avant l'injection async.
#[derive(Debug, Clone)]
struct PlaceholderQueue;

#[async_trait]
impl Queue for PlaceholderQueue {
    async fn get(&self, _id: JobId) -> Result<Option<JobInfo>, QueueError> {
        Ok(None)
    }

    async fn enqueue(&self, _job: NewJob) -> Result<JobId, QueueError> {
        // Placeholder : retourne ID 0 — signale l'absence de queue réelle.
        // En prod, ce chemin est impossible car `with_queue_path` est toujours
        // appelé avant tout enqueue.
        Ok(0)
    }

    async fn lease(
        &self,
        _kinds: &[&str],
        _duration: Duration,
    ) -> Result<Option<LeasedJob>, QueueError> {
        Ok(None)
    }

    async fn complete(&self, _id: JobId) -> Result<(), QueueError> {
        Ok(())
    }

    async fn fail(&self, _id: JobId, _err: &str) -> Result<(), QueueError> {
        Ok(())
    }

    async fn extend_lease(&self, _id: JobId, _dur: Duration) -> Result<(), QueueError> {
        Ok(())
    }

    async fn depth(&self) -> Result<u64, QueueError> {
        Ok(0)
    }

    async fn oldest_age_secs(&self) -> Result<u64, QueueError> {
        Ok(0)
    }
}

// ── NoopQueueStore — placeholder QueueStore (F-16) ───────────────────────────

/// Placeholder [`QueueStore`] (v81 trait) pour les constructeurs sync.
///
/// Retourne `NotFound` ou vide sur toutes les opérations.
/// Remplacé en production par `GradatumQueue` via `with_job_store`.
///
/// Ce type est privé à `state.rs` — non exposé dans l'API publique.
struct NoopQueueStore;

#[async_trait]
impl QueueStore for NoopQueueStore {
    async fn enqueue(
        &self,
        _job: gradatum_core::JobRecord,
    ) -> Result<ulid::Ulid, gradatum_core::QueueError> {
        Err(gradatum_core::QueueError::Storage(
            "NoopQueueStore : pas de job_store câblé — utiliser with_job_store".into(),
        ))
    }

    async fn dequeue(&self) -> Result<Option<gradatum_core::JobRecord>, gradatum_core::QueueError> {
        Ok(None)
    }

    async fn get(
        &self,
        _id: ulid::Ulid,
    ) -> Result<Option<gradatum_core::JobRecord>, gradatum_core::QueueError> {
        Ok(None)
    }

    async fn complete(
        &self,
        _id: ulid::Ulid,
        _result: gradatum_core::JobResult,
    ) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn fail(
        &self,
        _id: ulid::Ulid,
        _err: &str,
        _attempt: u32,
    ) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn cancel(&self, _id: ulid::Ulid) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn fail_dlq(&self, _id: ulid::Ulid, _err: &str) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn find_awaiting(
        &self,
        _job_id: ulid::Ulid,
    ) -> Result<Vec<gradatum_core::JobRecord>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    async fn set_pending(&self, _id: ulid::Ulid) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn recover_stale_leases(
        &self,
        _ttl: std::time::Duration,
    ) -> Result<Vec<ulid::Ulid>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    async fn cancel_expired_deadlines(
        &self,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ulid::Ulid>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    async fn promote_retries(
        &self,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ulid::Ulid>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    async fn schedule_retry(
        &self,
        _id: ulid::Ulid,
        _at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), gradatum_core::QueueError> {
        Ok(())
    }

    async fn list(
        &self,
        _filter: gradatum_core::JobFilter,
    ) -> Result<Vec<gradatum_core::JobRecord>, gradatum_core::QueueError> {
        Ok(vec![])
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<gradatum_core::QueueEvent> {
        // Canal vide — aucun event émis par le noop store.
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

// ── NoopApiKeyStore ───────────────────────────────────────────────────────────

/// Store d'API keys no-op pour les constructeurs sync et les tests qui n'exercent pas
/// le flow `/auth/exchange`.
///
/// Toute tentative de vérification retourne `NotFound`. Remplacé en production
/// par `SqliteApiKeyStore` via `AppState::with_api_keys_path`.
///
/// Ce type est privé à `state.rs` — non exposé dans l'API publique du crate.
struct NoopApiKeyStore;

#[async_trait]
impl ApiKeyStore for NoopApiKeyStore {
    async fn create(
        &self,
        _owner: &str,
        _scopes: Vec<String>,
        _tenant_id: String,
        _description: Option<String>,
    ) -> Result<gradatum_acl_auth::ApiKeyMaterial, gradatum_acl_auth::ApiKeyError> {
        Err(gradatum_acl_auth::ApiKeyError::Crypto(
            "NoopApiKeyStore : create() non supporté — câbler SqliteApiKeyStore via with_api_keys_path".into(),
        ))
    }

    async fn verify(
        &self,
        _secret: &str,
    ) -> Result<gradatum_acl_auth::ApiKey, gradatum_acl_auth::ApiKeyError> {
        Err(gradatum_acl_auth::ApiKeyError::NotFound)
    }

    async fn list(
        &self,
        _include_revoked: bool,
    ) -> Result<Vec<gradatum_acl_auth::ApiKey>, gradatum_acl_auth::ApiKeyError> {
        Ok(vec![])
    }

    async fn revoke(&self, _prefix: &str) -> Result<(), gradatum_acl_auth::ApiKeyError> {
        Err(gradatum_acl_auth::ApiKeyError::NotFound)
    }

    async fn rotate(
        &self,
        _prefix: &str,
    ) -> Result<gradatum_acl_auth::ApiKeyMaterial, gradatum_acl_auth::ApiKeyError> {
        Err(gradatum_acl_auth::ApiKeyError::NotFound)
    }
}

// ── PlaceholderRegistry ───────────────────────────────────────────────────────

/// Registry placeholder pour les constructeurs sync (`with_jwt`, `with_jwt_and_acl`).
///
/// Retourne 0/0 pour `tenant_count`/`locus_count` et no-op pour `ensure_tenant`.
/// Remplacé immédiatement par `Vault` via `with_vault_path` en production.
///
/// Ce type est privé à `state.rs` — non exposé dans l'API publique du crate.
/// Il permet aux constructeurs sync d'initialiser `AppState` avant l'injection async.
#[derive(Debug, Clone)]
struct PlaceholderRegistry;

#[async_trait]
impl Registry for PlaceholderRegistry {
    async fn tenant_count(&self) -> Result<u32, GradatumError> {
        // Placeholder : retourne 0 — signale l'absence de vault réel.
        // En prod, ce chemin est impossible car `with_vault_path` est toujours
        // appelé avant tout accès aux counts.
        Ok(0)
    }

    async fn locus_count(&self) -> Result<u32, GradatumError> {
        Ok(0)
    }

    async fn ensure_tenant(&self, _tenant_id: &str) -> Result<(), GradatumError> {
        Ok(())
    }

    async fn read_note_by_id(
        &self,
        note_id: &str,
    ) -> Result<gradatum_core::note::Note, GradatumError> {
        // Placeholder : retourne NoteNotFound — en prod `with_vault_path` injecte Vault réel.
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: ULID invalide: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }
}

// ── AppState ──────────────────────────────────────────────────────────────────

/// État partagé de l'application, clonable via `Arc<AppState>`.
///
/// Champs:
/// - `started_at`, `version`, `build_sha` : utilisés par /health (T10).
/// - `metrics` : métriques Prometheus canal latéral :19091 (T11, caveat C7).
/// - `jwt` : service JWT Ed25519 — vérifie les bearers entrants (T9).
/// - `acl` : moteur ACL compilé (T6). Initialisé avec le preset vide (default deny).
/// - `revocation` : store de révocation JWT (T4). InMemory par défaut (dev).
/// - `vault` : registry vault — `PlaceholderRegistry` avant injection ;
///   `Vault` en production (via `with_vault_path`). T2 P2.0c.
/// - `search` : stub search — remplacé par la vraie lib en T3 P2.0c.
/// - `queue` : queue de jobs. `PlaceholderQueue` avant injection ;
///   `SqliteQueue` en production (via `with_queue_path`).
#[allow(dead_code)]
#[derive(Clone)]
pub struct AppState {
    /// Instant de démarrage du serveur (pour le calcul d'uptime dans /health).
    pub started_at: Instant,
    /// Heure système de démarrage (pour `started_at` RFC3339 dans /health).
    pub started_at_systime: SystemTime,
    /// Version du binaire (depuis CARGO_PKG_VERSION à la compilation).
    pub version: &'static str,
    /// SHA du commit de build (depuis BUILD_SHA env à la compilation, sinon "unknown").
    pub build_sha: &'static str,
    /// Métriques Prometheus — canal latéral loopback :19091 (T11, caveat C7).
    pub metrics: AppMetrics,
    /// Service JWT Ed25519 pour vérification des bearers entrants (T9).
    ///
    /// Partagé via `Arc` — immuable après init au boot. `verify` est `&self`.
    pub jwt: Arc<JwtService>,
    /// Moteur ACL compilé (deny-wins, preset TOML). Initialisé avec preset vide = default deny.
    pub acl: Arc<AclEngine>,
    /// Store de révocation JWT. InMemory par défaut ; SqliteRevocationStore en prod.
    pub revocation: Arc<dyn RevocationStore>,
    /// Registry vault — `PlaceholderRegistry` avant `with_vault_path` ;
    /// `Vault` en production (T2 P2.0c).
    pub vault: Arc<dyn Registry>,
    /// Index de recherche — façade `dyn Index` (DocumentStore+IndexStore+VectorStore).
    ///
    /// Étape 0.2a : type concret effacé, `Arc<dyn Index>` permet le dispatch dynamique.
    /// En production, injecté via `with_search_path` (SqliteIndex en dessous).
    /// En dev/test, placeholder in-memory.
    pub search: Arc<dyn Index>,
    /// Queue de jobs async. `PlaceholderQueue` avant injection ;
    /// `SqliteQueue` en production via `with_queue_path` (T1 P2.0c).
    pub queue: Arc<dyn Queue>,
    /// Sink d'audit JSONL. `NoopAuditSink` par défaut (tests + dev) ;
    /// `JsonlFileSink` en production via `with_audit_dir` (T7 P2.0c, caveat C-aud-4).
    pub audit: Arc<dyn AuditSink>,
    /// Store d'API keys. `NoopApiKeyStore` par défaut ;
    /// `SqliteApiKeyStore` en production via `with_api_keys_path` (AUTH-T5).
    ///
    /// Utilisé par le handler `/auth/exchange` pour vérifier les clés entrantes.
    pub api_keys: Arc<dyn ApiKeyStore>,
    /// Embedder pour génération d'embeddings async (Phase 2.1.1 alpha.8).
    ///
    /// `Noop(384)` par défaut — câblé vers `HttpEmbedder` en production si `cfg.embed.enabled`.
    /// Utilisé par le worker (via `AppState` partagé) pour `embed_note` (Task 9+10).
    pub embedder: Arc<dyn Embedder>,
    /// Reranker cross-encoder pour le post-ranking vault_search (alpha.12 Task 14).
    ///
    /// `NoopReranker` par défaut (préserve l'ordre composite) ; `JinaOnnxReranker`
    /// activé en production via la feature `onnx-reranker` + binding `with_reranker`.
    pub reranker: Arc<dyn gradatum_search::Reranker>,
    /// QueueStore v81 (trait [`gradatum_core::QueueStore`]) pour les endpoints F-16.
    ///
    /// Distinct de `queue` (trait [`Queue`] déprécié) — cohabitation v0.2.0 pendant la
    /// transition vers le nouveau trait QueueStore (ARCH-D15).
    ///
    /// `NoopQueueStore` par défaut — câblé via `with_job_store` en production.
    /// En production, injecté depuis `GradatumQueue` (via `gradatum-queue`).
    pub job_store: Arc<dyn QueueStore>,
    /// Pool SQLite partagé pour la table d'idempotence (migration 008, F-16).
    ///
    /// `None` par défaut — câblé via `with_job_store_pool` en production.
    /// Quand `None`, les endpoints F-16 retournent 501 Not Implemented sur les opérations
    /// qui nécessitent le pool (Idempotency-Key).
    pub jobs_pool: Option<sqlx::SqlitePool>,
    /// Store append-only pour la table `event_log` (B1 tranche v0.3.0).
    ///
    /// `None` par défaut — câblé via `with_event_log_path` en production (même DB index.db).
    /// Quand `None`, l'endpoint `POST /api/v1/event-log` retourne 503 Service Unavailable.
    ///
    /// Design : connexion dédiée sur index.db (WAL, multi-connexion safe).
    /// Rétention bornée via tâche tokio interval (6h par défaut, config `[event_log]`).
    pub event_log: Option<EventLogStore>,
}

impl AppState {
    /// Crée un nouvel état avec l'instant de démarrage courant.
    ///
    /// L'ACL est initialisée avec un preset vide (default deny pour tout).
    /// Le RevocationStore est un InMemory (dev — voir caveat C2 pour la prod).
    /// Le JwtService est initialisé avec une clé éphémère (dev/test — WARN loggé au boot).
    /// La queue est un `PlaceholderQueue` — remplacée par `SqliteQueue` via `with_queue_path`.
    ///
    /// En production, enchaîner avec `.with_queue_path(&queue_path).await?`.
    pub fn new() -> Self {
        // Clé éphémère : acceptable en dev/test uniquement.
        // En production, utiliser `with_jwt(jwt_service)` après chargement PEM.
        let jwt = JwtService::new_ephemeral();
        Self::with_jwt(jwt)
    }

    /// Crée un état avec un `JwtService` explicite (pour la production).
    ///
    /// Utilisé dans `main.rs` après chargement de la clé Ed25519 depuis config.
    /// La `queue` est un `PlaceholderQueue` — enchaîner avec `with_queue_path` immédiatement.
    pub fn with_jwt(jwt: JwtService) -> Self {
        // Preset vide = aucun consumer → AclEngine retourne DenyImplicit pour tout.
        // Les handlers exigent Allow — tout token sans consumer configuré sera FORBIDDEN.
        let acl = AclEngine::from_preset_str("")
            .expect("le preset ACL vide est toujours valide — invariant statique");
        Self {
            started_at: Instant::now(),
            started_at_systime: SystemTime::now(),
            version: env!("CARGO_PKG_VERSION"),
            build_sha: option_env!("BUILD_SHA").unwrap_or("unknown"),
            metrics: AppMetrics::new(),
            jwt: Arc::new(jwt),
            acl: Arc::new(acl),
            revocation: Arc::new(InMemoryRevocationStore::new()),
            vault: Arc::new(PlaceholderRegistry),
            search: placeholder_search(),
            queue: Arc::new(PlaceholderQueue),
            audit: Arc::new(NoopAuditSink),
            api_keys: Arc::new(NoopApiKeyStore),
            embedder: Arc::new(NoopEmbedder::new(384)),
            reranker: Arc::new(gradatum_search::NoopReranker),
            job_store: Arc::new(NoopQueueStore),
            jobs_pool: None,
            event_log: None,
        }
    }

    /// Crée un état avec un `JwtService` et un `AclEngine` explicites (pour les tests).
    ///
    /// Utilisé dans les tests d'intégration (shape parity, write handlers) pour injecter
    /// un preset ACL autorisant le consumer de test.
    ///
    /// En production, l'ACL est chargée depuis config — utiliser [`AppState::with_jwt`].
    /// La `queue` est un `PlaceholderQueue` par défaut — injecter une queue réelle si besoin
    /// via `with_queue` (ex. `SqliteQueue::in_memory()`) ou `with_queue_path`.
    #[allow(dead_code)] // Utilisé dans v1-parity-tests (crate externe), invisible au dead_code lint binaire.
    pub fn with_jwt_and_acl(jwt: JwtService, acl: AclEngine) -> Self {
        Self {
            started_at: Instant::now(),
            started_at_systime: SystemTime::now(),
            version: env!("CARGO_PKG_VERSION"),
            build_sha: option_env!("BUILD_SHA").unwrap_or("unknown"),
            metrics: AppMetrics::new(),
            jwt: Arc::new(jwt),
            acl: Arc::new(acl),
            revocation: Arc::new(InMemoryRevocationStore::new()),
            vault: Arc::new(PlaceholderRegistry),
            search: placeholder_search(),
            queue: Arc::new(PlaceholderQueue),
            audit: Arc::new(NoopAuditSink),
            api_keys: Arc::new(NoopApiKeyStore),
            embedder: Arc::new(NoopEmbedder::new(384)),
            reranker: Arc::new(gradatum_search::NoopReranker),
            job_store: Arc::new(NoopQueueStore),
            jobs_pool: None,
            event_log: None,
        }
    }

    /// Remplace la queue par une implémentation réelle.
    ///
    /// Pattern builder : `AppState::new().with_queue(Arc::new(SqliteQueue::in_memory().await?))`
    /// Utilisé dans les tests d'intégration pour injecter une queue in-memory.
    #[allow(dead_code)] // API publique — utilisée dans les tests d'intégration.
    pub fn with_queue(mut self, queue: Arc<dyn Queue>) -> Self {
        self.queue = queue;
        self
    }

    /// Ouvre une `SqliteQueue` sur `path` et l'injecte dans l'état (production wiring).
    ///
    /// Retourne une erreur si la base SQLite ne peut pas être ouverte ou migrée.
    /// Utilisé dans `main.rs` pour le câblage production (`T1 P2.0c`).
    ///
    /// # Exemple
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // Dans un contexte tokio async :
    /// // let state = AppState::new()
    /// //     .with_queue_path(std::path::Path::new("/var/lib/gradatum/queue.db"))
    /// //     .await
    /// //     .expect("queue init failed");
    /// ```
    pub async fn with_queue_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use gradatum_queue::SqliteQueue;
        let queue = SqliteQueue::new(path).await?;
        self.queue = Arc::new(queue);
        Ok(self)
    }

    /// Ouvre (ou crée) un `SqliteIndex` sur `path` et l'injecte dans l'état (production wiring).
    ///
    /// Retourne une erreur si le fichier SQLite est inaccessible ou si les migrations échouent.
    /// Utilisé dans `main.rs` pour le câblage production (`T3 P2.0c`).
    ///
    /// # Exemple
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // Dans un contexte tokio async :
    /// // let state = AppState::new()
    /// //     .with_search_path(std::path::Path::new("/var/lib/gradatum/index.db"))
    /// //     .await
    /// //     .expect("search init failed");
    /// ```
    /// Câblé dans main.rs T10 — pointé vers vault/.gradatum/index.db (index commun avec le worker).
    pub async fn with_search_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        let index = SqliteIndex::open(path).await?;
        self.search = Arc::new(index) as Arc<dyn Index>;
        Ok(self)
    }

    /// Injecte un `Arc<dyn Registry>` pré-construit comme registry vault.
    ///
    /// Utilisé dans les tests d'intégration pour injecter directement un `Vault` déjà créé
    /// (typiquement via `Vault::create` ou `Vault::open` dans un TempDir).
    /// En production, utiliser `with_vault_path`.
    #[allow(dead_code)]
    pub fn with_vault_arc(mut self, vault: Arc<dyn Registry>) -> Self {
        self.vault = vault;
        self
    }

    /// Injecte un `Arc<dyn AuditSink>` pré-construit comme sink d'audit.
    ///
    /// Utilisé dans les tests d'intégration pour injecter directement un sink d'audit.
    /// En production, utiliser `with_audit_dir`.
    #[allow(dead_code)]
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = audit;
        self
    }

    /// Ouvre (ou crée) un `Vault` sur `path` et l'injecte comme registry dans l'état.
    ///
    /// Si le layout vault n'existe pas encore (`path/.gradatum/` absent), `Vault::create`
    /// initialise le layout complet avant d'ouvrir l'index SQLite.
    ///
    /// Retourne une erreur si le répertoire est sur NFS ou si l'index est inaccessible.
    /// Utilisé dans `main.rs` pour le câblage production (`T2 P2.0c`).
    ///
    /// # Exemple
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // Dans un contexte tokio async :
    /// // let state = AppState::new()
    /// //     .with_vault_path(std::path::Path::new("/var/lib/gradatum/vault"))
    /// //     .await
    /// //     .expect("vault init failed");
    /// ```
    pub async fn with_vault_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use gradatum_core::scope::VaultId;
        // Si le layout vault n'existe pas, on le crée (idempotent).
        // `Vault::create` est safe si les répertoires existent déjà.
        let vault = if path.join(".gradatum").exists() {
            gradatum_vault::Vault::open(path).await?
        } else {
            gradatum_vault::Vault::create(path, VaultId::new("main")).await?
        };
        self.vault = Arc::new(vault);
        Ok(self)
    }

    /// Ouvre (ou crée) un `SqliteApiKeyStore` sur `path` et l'injecte dans l'état (AUTH-T5).
    ///
    /// Remplace le `NoopApiKeyStore` par un store SQLite persistant.
    /// Requis pour que le handler `/auth/exchange` fonctionne en production.
    ///
    /// # Exemple
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // Dans un contexte tokio async :
    /// // let state = AppState::new()
    /// //     .with_api_keys_path(std::path::Path::new("/var/lib/gradatum/db/api_keys.sqlite"))
    /// //     .await
    /// //     .expect("api_keys store init failed");
    /// ```
    pub async fn with_api_keys_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        let store = SqliteApiKeyStore::init(path)
            .await
            .map_err(|e| anyhow::anyhow!("SqliteApiKeyStore init failed: {e}"))?;
        self.api_keys = Arc::new(store);
        Ok(self)
    }

    /// Ouvre (ou crée) un `SqliteRevocationStore` sur `path` et l'injecte dans l'état.
    ///
    /// Remplace le `InMemoryRevocationStore` (DEV ONLY) par une implémentation persistante.
    /// Le WARN `InMemoryRevocationStore activé — DEV ONLY` disparaît après cet appel.
    ///
    /// Utilisé dans `main.rs` quand `config.auth.revocation_store == "sqlite"` (AUTH-T6).
    ///
    /// # Exemple
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // Dans un contexte tokio async :
    /// // let state = AppState::new()
    /// //     .with_revocation_path(std::path::Path::new("/var/lib/gradatum/db/revocation.sqlite"))
    /// //     .await
    /// //     .expect("revocation store init failed");
    /// ```
    pub async fn with_revocation_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use gradatum_auth::revocation::SqliteRevocationStore;
        let store = SqliteRevocationStore::new(path)
            .await
            .map_err(|e| anyhow::anyhow!("SqliteRevocationStore init failed: {e}"))?;
        self.revocation = Arc::new(store);
        Ok(self)
    }

    /// Charge un preset ACL depuis `path` et l'injecte dans l'état (production wiring).
    ///
    /// Le fichier `path` doit être un TOML de preset ACL (bearer.toml).
    /// Si le fichier est absent, illisible ou si le chemin est vide → fallback DENY-ALL
    /// (comportement sécurisé : `AclEngine::from_preset_str("")`).
    ///
    /// Utilisé dans `main.rs` après lecture de `cfg.acl.preset_path` (E1 fix P2.0c-bis).
    ///
    /// # Exemple
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // let state = AppState::new()
    /// //     .with_acl_preset_path(std::path::Path::new("/var/lib/gradatum/config/bearer.toml"));
    /// ```
    pub fn with_acl_preset_path(mut self, path: &std::path::Path) -> Self {
        if path.as_os_str().is_empty() {
            tracing::warn!("ACL preset absent ou illisible — fallback DENY-ALL (chemin vide)");
            return self;
        }
        match std::fs::read_to_string(path) {
            Ok(content) => match AclEngine::from_preset_str(&content) {
                Ok(engine) => {
                    tracing::info!(path = %path.display(), "AclEngine chargé depuis preset");
                    self.acl = Arc::new(engine);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "ACL preset illisible (parse error) — fallback DENY-ALL"
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "ACL preset absent ou illisible — fallback DENY-ALL"
                );
            }
        }
        self
    }

    /// Crée un `JsonlFileSink` sur `dir` et l'injecte comme sink d'audit (production wiring).
    ///
    /// Le répertoire est créé automatiquement au premier appel à `record`.
    /// Remplace le `NoopAuditSink` par défaut.
    ///
    /// Utilisé dans `main.rs` pour le câblage production (`T7 P2.0c`).
    /// Phase 2.1 : câblage main.rs effectif différé — méthode prête, pas encore appelée.
    ///
    /// # Exemple
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // Dans un contexte tokio async :
    /// // let state = AppState::new()
    /// //     .with_audit_dir(std::path::Path::new("/var/lib/gradatum/audit"))
    /// //     .await
    /// //     .expect("audit sink init failed");
    /// ```
    #[allow(dead_code)]
    pub async fn with_audit_dir(mut self, dir: &std::path::Path) -> anyhow::Result<Self> {
        use crate::audit_jsonl::JsonlFileSink;
        let sink = JsonlFileSink::new(dir.to_path_buf());
        // Créer le répertoire dès le wiring pour détecter les erreurs de permissions tôt.
        tokio::fs::create_dir_all(dir).await?;
        self.audit = Arc::new(sink);
        Ok(self)
    }

    /// Injecte un embedder pré-construit (production wiring T8 P2.1.1).
    ///
    /// Remplace le `Noop(384)` par défaut par un `HttpEmbedder` configuré via `cfg.embed`.
    /// Utilisé dans `main.rs` si `cfg.embed.enabled` est vrai.
    ///
    /// Pattern builder : `state.with_embedder(Arc::new(HttpEmbedder::new(...)))`
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Remplace le reranker par défaut (`NoopReranker`) par une impl alternative.
    ///
    /// alpha.12 Task 14 — utilisé pour câbler `JinaOnnxReranker` quand la feature
    /// `onnx-reranker` est activée et qu'un fichier modèle ONNX est disponible.
    ///
    /// Pattern builder : `state.with_reranker(Arc::new(JinaOnnxReranker::from_file(p)?))`
    #[allow(dead_code)]
    pub fn with_reranker(mut self, reranker: Arc<dyn gradatum_search::Reranker>) -> Self {
        self.reranker = reranker;
        self
    }

    /// Injecte un [`QueueStore`] (v81 trait) et le pool SQLite pour les endpoints F-16.
    ///
    /// Pattern builder : `state.with_job_store(Arc::new(SqliteQueueStore::new(pool.clone())), pool)`
    ///
    /// En production, appelé depuis `main.rs` après initialisation du pool WAL
    /// sur `cfg.storage.root/db/queue.sqlite` (même fichier que le worker).
    /// Le pool est requis pour les opérations d'idempotence (table `gradatum_idempotency`).
    pub fn with_job_store(mut self, store: Arc<dyn QueueStore>, pool: SqlitePool) -> Self {
        self.job_store = store;
        self.jobs_pool = Some(pool);
        self
    }

    /// Ouvre un `EventLogStore` sur `path` et l'injecte dans l'état (production wiring).
    ///
    /// `path` doit pointer vers le même fichier `index.db` que `with_search_path`
    /// (la migration 0006 y ajoute la table `event_log`).
    ///
    /// Une connexion dédiée est ouverte en mode WAL (safe multi-connexion SQLite).
    /// Le store est utilisé par `POST /api/v1/event-log` et la tâche de rétention.
    ///
    /// # Erreurs
    ///
    /// Retourne une erreur si le fichier SQLite est inaccessible.
    pub async fn with_event_log_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use crate::event_log_store::EventLogStore;
        let store = EventLogStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("EventLogStore init failed: {e}"))?;
        self.event_log = Some(store);
        Ok(self)
    }

    /// Injecte un `EventLogStore` pré-construit (pour les tests d'intégration).
    ///
    /// Pattern : `state.with_event_log(EventLogStore::open_in_memory().await?)`
    #[cfg(test)]
    pub fn with_event_log(mut self, store: EventLogStore) -> Self {
        self.event_log = Some(store);
        self
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
