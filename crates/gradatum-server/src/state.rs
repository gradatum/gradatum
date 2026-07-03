//! Shared application state — Arc-able and cloneable across handlers.
//!
//! `AppState::search` is an `Arc<dyn Index>` — wired via `with_search_path` in production
//! and backed by an in-memory placeholder in sync constructors.
//! `AppState::vault` is `Arc<dyn gradatum_vault::Registry>` — wired via
//! `with_vault_path` in production and `PlaceholderRegistry` in sync constructors.
//!
//! `AppState::jwt` (`JwtService`) and `AppState::acl` (`AclEngine`) are real
//! implementations — initialised with safe defaults for tests.
//!
//! `AppState::audit` defaults to `NoopAuditSink` — wired via `with_audit_dir`
//! in production to produce rotating JSONL files.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant, SystemTime};

use gradatum_embed::{Embedder, Noop as NoopEmbedder};

use async_trait::async_trait;
use gradatum_acl_auth::{ApiKeyStore, SqliteApiKeyStore};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_auth::revocation::{InMemoryRevocationStore, RevocationStore};
use gradatum_core::QueueStore;
use gradatum_core::audit::http::AuditSink;
use gradatum_core::error::GradatumError;
use gradatum_vault::Registry;
use sqlx::SqlitePool;

use gradatum_queue::{JobId, JobInfo, LeasedJob, NewJob, Queue, QueueError};

use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;

use crate::event_log_store::EventLogStore;
use crate::mcp_usage::McpToolCounters;
use crate::metrics::AppMetrics;
use crate::read_usage_store::ReadUsageCounterStore;

// ── NoopAuditSink (state.rs) ──────────────────────────────────────────────────

/// No-op audit sink for tests and sync constructors.
///
/// Replaced by `JsonlFileSink` in production via `AppState::with_audit_dir`.
/// Distinct from the `NoopAuditSink` in `gradatum-worker` — each crate keeps its own
/// to avoid cross-crate coupling.
struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    /// Does nothing — events are silently discarded.
    async fn record(
        &self,
        _event: gradatum_core::audit::http::HttpAuditEvent,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

// ── Placeholders internes ──────────────────────────────────────────────────────

/// Initialises an in-memory `SqliteIndex` for sync constructors.
///
/// Called by `with_jwt` and `with_jwt_and_acl` to populate the `search` field
/// before injection via `with_search_path`.
///
/// Uses a dedicated thread with its own Tokio runtime to work around the
/// `block_on` restriction inside a `current_thread` runtime (used by `#[tokio::test]`).
/// The thread is joined immediately — no leak.
///
/// Returns `Arc<dyn Index>` — the concrete `SqliteIndex` type is erased immediately
/// to match the `AppState.search: Arc<dyn Index>` field.
///
/// # Panics
///
/// Panics only if the in-memory SQLite initialisation fails — impossible in practice.
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

/// Queue placeholder for sync constructors (`with_jwt`, `with_jwt_and_acl`).
///
/// Replaced immediately by `SqliteQueue` via `with_queue_path` in production
/// or by `SqliteQueue::in_memory()` via `with_queue` in tests.
///
/// Private to `state.rs` — not exposed in the crate's public API.
/// Allows sync constructors to initialise `AppState` before async injection.
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

/// Placeholder [`QueueStore`] for sync constructors.
///
/// Returns `NotFound` or empty on all operations.
/// Replaced in production by `GradatumQueue` via `with_job_store`.
///
/// Private to `state.rs` — not exposed in the crate's public API.
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

/// No-op API key store for sync constructors and tests that do not exercise
/// the `/auth/exchange` flow.
///
/// Every verification attempt returns `NotFound`. Replaced in production
/// by `SqliteApiKeyStore` via `AppState::with_api_keys_path`.
///
/// Private to `state.rs` — not exposed in the crate's public API.
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

/// Registry placeholder for sync constructors (`with_jwt`, `with_jwt_and_acl`).
///
/// Returns 0/0 for `tenant_count`/`locus_count` and no-ops `ensure_tenant`.
/// Replaced immediately by `Vault` via `with_vault_path` in production.
///
/// Private to `state.rs` — not exposed in the crate's public API.
/// Allows sync constructors to initialise `AppState` before async injection.
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

    async fn history_versions(&self, _note_id: &str) -> Result<Vec<i64>, GradatumError> {
        // Placeholder — pas de vault réel.
        Ok(Vec::new())
    }

    async fn history_get(
        &self,
        note_id: &str,
        _ts_ms: i64,
    ) -> Result<gradatum_core::note::Note, GradatumError> {
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: ULID invalide: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn history_restore(&self, _note_id: &str, _ts_ms: i64) -> Result<String, GradatumError> {
        Err(GradatumError::Storage(
            "history_restore: placeholder vault — injection requise".to_string(),
        ))
    }

    async fn history_diff(
        &self,
        _note_id: &str,
        _a: &str,
        _b: &str,
    ) -> Result<Vec<String>, GradatumError> {
        Err(GradatumError::Storage(
            "history_diff: placeholder vault — injection requise".to_string(),
        ))
    }

    async fn update_note_status(
        &self,
        note_id: &str,
        _target: gradatum_core::status::NoteStatus,
        _reason: Option<String>,
    ) -> Result<(), GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: ULID invalide: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn add_tags(&self, note_id: &str, _tags: &[String]) -> Result<(), GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: ULID invalide: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn move_locus(
        &self,
        note_id: &str,
        _new_locus: &gradatum_core::scope::LocusId,
    ) -> Result<(), GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        use gradatum_core::identity::NoteId;
        use ulid::Ulid;
        let ulid = Ulid::from_string(note_id)
            .map_err(|e| GradatumError::Storage(format!("placeholder: ULID invalide: {e}")))?;
        Err(GradatumError::NoteNotFound(NoteId(ulid)))
    }

    async fn write_note_with_id_internal(
        &self,
        _frontmatter: gradatum_core::frontmatter::Frontmatter,
        _body: String,
        _id: gradatum_core::identity::NoteId,
    ) -> Result<gradatum_core::note::Note, GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        Err(GradatumError::Storage(
            "write_note_with_id_internal: placeholder vault — injection requise".to_string(),
        ))
    }

    async fn delete_note_by_id(
        &self,
        id: gradatum_core::identity::NoteId,
    ) -> Result<(), GradatumError> {
        // Placeholder — en prod `with_vault_path` injecte Vault réel.
        Err(GradatumError::NoteNotFound(id))
    }
}

// ── ReadUsageAccumulators ─────────────────────────────────────────────────────

/// Compteurs AtomicU64 en mémoire pour les 5 read-paths instrumentés (télémétrie v0.5.3).
///
/// ## Design
///
/// Chaque handler read-path incrémente son compteur via `fetch_add(1, Ordering::Relaxed)`.
/// Coût hot-path : ~0 (instruction atomique, aucun I/O).
///
/// La tâche flush (interval 60s dans `main.rs`) swaps les compteurs à 0 (`swap(0, Relaxed)`)
/// et flushes les valeurs accumulées dans `read_usage_counters` (UPSERT par window_h).
///
/// ## Thread-safety
///
/// `Arc<ReadUsageAccumulators>` — partagé entre tous les handlers et la tâche flush.
/// `AtomicU64` est `Send + Sync` — safe pour un partage multi-thread.
///
/// ## Ordering::Relaxed
///
/// Suffisant ici : seule la valeur du compteur compte.
/// Aucun ordering cross-thread requis (pas d'effets mémoire à synchroniser avec le
/// flush — le swap-et-reset est l'unique point de synchronisation, géré par atomicité).
#[derive(Debug, Default)]
pub struct ReadUsageAccumulators {
    /// Compteur de hits pour `POST /api/v1/vault_search`.
    pub vault_search: AtomicU64,
    /// Compteur de hits pour `POST /api/v1/vault_read`.
    pub vault_read: AtomicU64,
    /// Compteur de hits pour `POST /api/v1/code_scope`.
    pub code_scope: AtomicU64,
    /// Compteur de hits pour `POST /api/v1/vault_timeline`.
    pub vault_timeline: AtomicU64,
    /// Compteur de hits pour `GET /api/v1/lessons/recall`.
    pub lessons_recall: AtomicU64,
}

// ── AppState ──────────────────────────────────────────────────────────────────

/// Shared application state, cloneable via `Arc<AppState>`.
///
/// Fields:
/// - `started_at`, `version`, `build_sha`: used by `/health`.
/// - `metrics`: Prometheus metrics on the sidecar port :19091.
/// - `jwt`: Ed25519 JWT service — verifies incoming bearer tokens.
/// - `acl`: compiled ACL engine. Initialised with an empty preset (default deny).
/// - `revocation`: JWT revocation store. In-memory by default (development).
/// - `vault`: vault registry — `PlaceholderRegistry` before injection;
///   `Vault` in production (via `with_vault_path`).
/// - `search`: search index — replaced by a real `SqliteIndex` in production.
/// - `queue`: job queue. `PlaceholderQueue` before injection;
///   `SqliteQueue` in production (via `with_queue_path`).
#[allow(dead_code)]
#[derive(Clone)]
pub struct AppState {
    /// Server start instant (used to compute uptime in `/health`).
    pub started_at: Instant,
    /// System time at startup (used for the RFC 3339 `started_at` field in `/health`).
    pub started_at_systime: SystemTime,
    /// Binary version (from `CARGO_PKG_VERSION` at compile time).
    pub version: &'static str,
    /// Build commit SHA (from the `BUILD_SHA` env var at compile time, or `"unknown"`).
    pub build_sha: &'static str,
    /// Prometheus metrics — sidecar loopback port :19091.
    pub metrics: AppMetrics,
    /// Ed25519 JWT service for verifying incoming bearer tokens.
    ///
    /// Shared via `Arc` — immutable after boot initialisation. `verify` takes `&self`.
    pub jwt: Arc<JwtService>,
    /// Compiled ACL engine (deny-wins, TOML preset). Initialised with an empty preset = default deny.
    pub acl: Arc<AclEngine>,
    /// JWT revocation store. In-memory by default; `SqliteRevocationStore` in production.
    pub revocation: Arc<dyn RevocationStore>,
    /// Vault registry — `PlaceholderRegistry` before `with_vault_path`;
    /// `Vault` in production.
    pub vault: Arc<dyn Registry>,
    /// Search index — `dyn Index` facade (`DocumentStore` + `IndexStore` + `VectorStore`).
    ///
    /// Concrete type erased; `Arc<dyn Index>` enables dynamic dispatch.
    /// In production, injected via `with_search_path` (backed by `SqliteIndex`).
    /// In dev/test, an in-memory placeholder.
    pub search: Arc<dyn Index>,
    /// Async job queue. `PlaceholderQueue` before injection;
    /// `SqliteQueue` in production via `with_queue_path`.
    pub queue: Arc<dyn Queue>,
    /// JSONL audit sink. `NoopAuditSink` by default (tests + dev);
    /// `JsonlFileSink` in production via `with_audit_dir`.
    pub audit: Arc<dyn AuditSink>,
    /// API key store. `NoopApiKeyStore` by default;
    /// `SqliteApiKeyStore` in production via `with_api_keys_path`.
    ///
    /// Used by the `/auth/exchange` handler to verify incoming keys.
    pub api_keys: Arc<dyn ApiKeyStore>,
    /// Embedder for async embedding generation.
    ///
    /// `Noop(384)` by default — wired to `HttpEmbedder` in production when `cfg.embed.enabled`.
    /// Used by the worker (via shared `AppState`) for `embed_note`.
    pub embedder: Arc<dyn Embedder>,
    /// Cross-encoder reranker for post-ranking in vault search.
    ///
    /// `NoopReranker` by default (preserves composite order); `JinaOnnxReranker`
    /// enabled in production via the `onnx-reranker` feature + `with_reranker`.
    pub reranker: Arc<dyn gradatum_search::Reranker>,
    /// [`gradatum_core::QueueStore`] for job endpoints.
    ///
    /// Distinct from `queue` (the deprecated [`Queue`] trait) — both coexist during
    /// the migration to the new `QueueStore` trait.
    ///
    /// `NoopQueueStore` by default — wired via `with_job_store` in production.
    /// In production, injected from `GradatumQueue` (via `gradatum-queue`).
    pub job_store: Arc<dyn QueueStore>,
    /// Shared SQLite pool for the idempotency table (migration 008).
    ///
    /// `None` by default — wired via `with_job_store_pool` in production.
    /// When `None`, job endpoints return 501 Not Implemented for operations
    /// that require the pool (Idempotency-Key).
    pub jobs_pool: Option<sqlx::SqlitePool>,
    /// Append-only store for the `event_log` table.
    ///
    /// `None` by default — wired via `with_event_log_path` in production (same `index.db`).
    /// When `None`, `POST /api/v1/event-log` returns 503 Service Unavailable.
    ///
    /// Uses a dedicated connection on `index.db` (WAL, multi-connection safe).
    /// Retention bounded via a Tokio interval task (6 h by default, `[event_log]` config).
    pub event_log: Option<EventLogStore>,

    /// Append-only store for the `session_trace` table (session-log Tier 1).
    ///
    /// `None` by default — wired via `with_session_trace_path` in production (same
    /// `index.db`). When `None`, `POST /api/v1/session-log/trace` returns 503 Service Unavailable.
    ///
    /// Uses a dedicated connection on `index.db` (WAL, multi-connection safe).
    /// Retention bounded to 90 days via a Tokio interval task (`[session_trace]` config).
    pub session_trace: Option<crate::session_trace_store::SessionTraceStore>,

    /// UPSERT latest-per-tenant store for the `proactive_surface` table (F-46, Active Recall).
    ///
    /// `None` by default — wired via `with_proactive_surface_path` in production (same
    /// `index.db`). When `None`, the proactive refresh task is skipped (noop).
    ///
    /// Uses a dedicated connection on `index.db` (WAL, multi-connection safe).
    /// Written by the `proactive_refresh_once` interval task.
    pub proactive_surface: Option<crate::proactive_surface_store::ProactiveSurfaceStore>,

    /// Sessions + feedback store pour le rappel proactif (F-46, Active Recall v0.7.1).
    ///
    /// `None` par défaut — câblé via `with_proactive_recall_path` en production (même
    /// `index.db`). Quand `None`, les handlers de feedback proactif retournent 503.
    ///
    /// Connexion WAL dédiée (safe multi-connexion SQLite).
    /// Written by `insert_session` and `record_feedback`.
    pub proactive_recall: Option<crate::proactive_recall_store::ProactiveRecallStore>,

    /// Trust-decay configuration for search scoring.
    ///
    /// Default: decay enabled (`distilled = 90 days`). Disable via `[scoring]` config
    /// (`trust_decay_enabled = false`). Wired via `with_scoring`.
    pub scoring: Arc<gradatum_search::TrustDecayConfig>,

    /// Path to the SQLite WAL file (`<index.db>-wal`).
    ///
    /// `None` before `with_search_path` (in-memory placeholder: no on-disk WAL).
    /// Wired in production to `<index.db>-wal` to expose the real `sqlite_wal_size_bytes`
    /// in `/health` and the dashboard. Unmeasurable WAL → `None` ⇒ honest "n/a"
    /// (never 0, which would falsely claim "healthy").
    pub wal_path: Option<std::path::PathBuf>,

    /// Store de compteurs d'usage des read-paths (télémétrie v0.5.3 #4).
    ///
    /// `None` par défaut — câblé via `with_read_usage_path` en production (même `index.db`).
    /// Flushed toutes les 60s par la tâche flush dans `main.rs`.
    pub read_usage: Option<ReadUsageCounterStore>,

    /// Accumulateurs AtomicU64 pour les 5 read-paths instrumentés.
    ///
    /// `Arc` partagé entre tous les handlers et la tâche flush.
    /// Les handlers incrémentent via `fetch_add(1, Ordering::Relaxed)` — coût ~0, aucun I/O.
    /// wired: AppState::with_jwt() → initialisé à 0 dès le boot.
    pub read_usage_accumulators: Arc<ReadUsageAccumulators>,

    /// Token de l'API interne server-to-worker (Wave 2, v0.5.3).
    ///
    /// `None` = listener interne désactivé (opt-in via `[internal_api]` config).
    /// wired: `gradatum-server/src/main.rs` + `internal/auth.rs`
    pub internal_api_token: Option<Arc<secrecy::SecretString>>,

    /// Compteurs atomiques par outil MCP — télémétrie feat/usage-telemetry-19091.
    ///
    /// Map fermée pré-peuplée (21 outils). `record(name)` est un no-op sur nom inconnu.
    /// Swappé toutes les 60s par la tâche flush pour UPSERT dans `read_usage_counters`
    /// (clés `mcp:<tool>`) et fan-out dans la famille Prometheus `mcp_tool_calls`.
    pub mcp_tool_counters: Arc<McpToolCounters>,

    /// In-memory cache of the skills index (section `"skills"`).
    ///
    /// `None` avant le premier appel `vault_context` avec `inject_skills=true`.
    /// Construit paresseusement (lazy) par `context::skills::build_skill_index` :
    /// scan SQL section `"skills"` + `embed_batch`. Stocké comme `Arc<SkillIndex>`
    /// pour cloner la référence hors du `RwLock` sans copier les vecteurs d'embedding.
    ///
    /// # Invalidation
    ///
    /// Aucun hook `vault_write` pour l'instant (ECON: rebuild lazy à chaque cache miss).
    /// Le cache persiste pour la durée de vie du serveur tant que le build réussit.
    pub skills_index: Arc<tokio::sync::RwLock<Option<Arc<crate::context::skills::SkillIndex>>>>,

    /// Configuration du serveur (intervalles des tâches, options avancées).
    ///
    /// Utilisé par `GET /api/v1/system/scheduled` pour exposer `interval_secs` via
    /// `task_interval_secs(name, &server_config)` — SSOT T4 (zéro divergence entre
    /// intervalles réels et intervalles rapportés).
    /// Défaut : `ServerConfig::default()` (valeurs de production par défaut).
    /// Wired via `AppState::with_server_config` dans `main.rs`.
    pub server_config: Arc<crate::config::ServerConfig>,

    /// Configuration for the context assembly pipeline.
    ///
    /// Governs `assemble_assembled`: default budget, top_n candidates, max skills,
    /// skills budget fraction, embed timeout.
    /// Defaults: budget=2000, top_n=50, max_skills=3, frac=0.15, embed=800ms.
    /// Wired via `AppState::with_context` in `main.rs`.
    pub context: Arc<crate::config::ContextConfig>,
}

impl AppState {
    /// Creates a new state with the current start instant.
    ///
    /// The ACL is initialised with an empty preset (default deny for everything).
    /// The revocation store is in-memory (development only).
    /// `JwtService` is initialised with an ephemeral key (dev/test — WARN logged at boot).
    /// The queue is a `PlaceholderQueue` — replaced by `SqliteQueue` via `with_queue_path`.
    ///
    /// In production, chain with `.with_queue_path(&queue_path).await?`.
    pub fn new() -> Self {
        // Clé éphémère : acceptable en dev/test uniquement.
        // En production, utiliser `with_jwt(jwt_service)` après chargement PEM.
        let jwt = JwtService::new_ephemeral();
        Self::with_jwt(jwt)
    }

    /// Creates a state with an explicit `JwtService` (for production).
    ///
    /// Used in `main.rs` after loading the Ed25519 key from config.
    /// The queue is a `PlaceholderQueue` — chain with `with_queue_path` immediately.
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
            session_trace: None,
            proactive_surface: None,
            proactive_recall: None,
            scoring: Arc::new(gradatum_search::TrustDecayConfig::default()),
            wal_path: None,
            read_usage: None,
            read_usage_accumulators: Arc::new(ReadUsageAccumulators::default()),
            internal_api_token: None,
            mcp_tool_counters: Arc::new(McpToolCounters::new()),
            skills_index: Arc::new(tokio::sync::RwLock::new(None)),
            server_config: Arc::new(crate::config::ServerConfig::default()),
            context: Arc::new(crate::config::ContextConfig::default()),
        }
    }

    /// Creates a state with explicit `JwtService` and `AclEngine` (for tests).
    ///
    /// Used in integration tests to inject an ACL preset that allows the test consumer.
    ///
    /// In production the ACL is loaded from config — use [`AppState::with_jwt`].
    /// The queue is a `PlaceholderQueue` by default — inject a real queue if needed
    /// via `with_queue` (e.g. `SqliteQueue::in_memory()`) or `with_queue_path`.
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
            session_trace: None,
            proactive_surface: None,
            proactive_recall: None,
            scoring: Arc::new(gradatum_search::TrustDecayConfig::default()),
            wal_path: None,
            read_usage: None,
            read_usage_accumulators: Arc::new(ReadUsageAccumulators::default()),
            internal_api_token: None,
            mcp_tool_counters: Arc::new(McpToolCounters::new()),
            skills_index: Arc::new(tokio::sync::RwLock::new(None)),
            server_config: Arc::new(crate::config::ServerConfig::default()),
            context: Arc::new(crate::config::ContextConfig::default()),
        }
    }

    /// Replaces the queue with a real implementation.
    ///
    /// Builder pattern: `AppState::new().with_queue(Arc::new(SqliteQueue::in_memory().await?))`
    /// Used in integration tests to inject an in-memory queue.
    #[allow(dead_code)] // API publique — utilisée dans les tests d'intégration.
    pub fn with_queue(mut self, queue: Arc<dyn Queue>) -> Self {
        self.queue = queue;
        self
    }

    /// Opens a `SqliteQueue` at `path` and injects it into the state (production wiring).
    ///
    /// Returns an error if the SQLite database cannot be opened or migrated.
    /// Used in `main.rs` for production wiring.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
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

    /// Opens (or creates) a `SqliteIndex` at `path` and injects it into the state (production wiring).
    ///
    /// Returns an error if the SQLite file is inaccessible or migrations fail.
    /// Used in `main.rs` for production wiring — pointed at `vault/.gradatum/index.db`
    /// (the shared index used by the worker).
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
    /// // let state = AppState::new()
    /// //     .with_search_path(std::path::Path::new("/var/lib/gradatum/index.db"))
    /// //     .await
    /// //     .expect("search init failed");
    /// ```
    #[allow(dead_code)]
    pub async fn with_search_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        let index = SqliteIndex::open(path).await?;
        self.search = Arc::new(index) as Arc<dyn Index>;
        // F-37 S1.3 / T12 — chemin WAL conventionnel SQLite : `<index.db>-wal`.
        // Exposé read-only pour `sqlite_wal_size_bytes` (/health + dashboard).
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        self.wal_path = Some(std::path::PathBuf::from(wal));
        Ok(self)
    }

    /// Ouvre un `SqliteIndex`, configure le chemin ANN (v0.5.3 ANN-5), puis injecte.
    ///
    /// Variante de `with_search_path` qui configure le moteur ANN AVANT de caster
    /// le `SqliteIndex` en `Arc<dyn Index>` (le downcast devient impossible après).
    ///
    /// Si `ann_backend = SqliteVec`, active le chemin ANN sur l'index.
    /// Si `ann_backend = BruteForce` (défaut), comportement identique à `with_search_path`.
    ///
    /// # Errors
    ///
    /// Retourne `Err` si l'ouverture SQLite ou les migrations échouent.
    pub async fn with_search_path_ann(
        mut self,
        path: &std::path::Path,
        ann_backend: crate::config::AnnBackend,
        ann_ef_search: u32,
    ) -> anyhow::Result<Self> {
        let index = SqliteIndex::open(path).await?;

        // Configure ANN AVANT le cast en dyn — seule fenêtre où le type concret
        // est encore accessible sans downcast.
        if ann_backend == crate::config::AnnBackend::SqliteVec {
            index.set_ann_enabled(true);
            index.set_ann_ef_search(ann_ef_search);
            tracing::info!(
                ef_search = ann_ef_search,
                "ANN sqlite-vec activé (ann_backend=sqlite_vec)"
            );
        }

        self.search = Arc::new(index) as Arc<dyn Index>;
        // F-37 S1.3 / T12 — chemin WAL.
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        self.wal_path = Some(std::path::PathBuf::from(wal));
        Ok(self)
    }

    /// Injects a pre-built `Arc<dyn Registry>` as the vault registry.
    ///
    /// Used in integration tests to inject a `Vault` already created
    /// (typically via `Vault::create` or `Vault::open` inside a `TempDir`).
    /// In production, use `with_vault_path`.
    #[allow(dead_code)]
    pub fn with_vault_arc(mut self, vault: Arc<dyn Registry>) -> Self {
        self.vault = vault;
        self
    }

    /// Injects a pre-built `Arc<dyn AuditSink>` as the audit sink.
    ///
    /// Used in integration tests to inject an audit sink directly.
    /// In production, use `with_audit_dir`.
    #[allow(dead_code)]
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = audit;
        self
    }

    /// Opens (or creates) a `Vault` at `path` and injects it as the registry.
    ///
    /// If the vault layout does not yet exist (`path/.gradatum/` absent), `Vault::create`
    /// initialises the full layout before opening the SQLite index.
    ///
    /// Returns an error if the directory is on NFS or the index is inaccessible.
    /// Used in `main.rs` for production wiring.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
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

    /// Opens (or creates) a `SqliteApiKeyStore` at `path` and injects it into the state.
    ///
    /// Replaces the `NoopApiKeyStore` with a persistent SQLite store.
    /// Required for the `/auth/exchange` handler to function in production.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
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

    /// Opens (or creates) a `SqliteRevocationStore` at `path` and injects it into the state.
    ///
    /// Replaces the `InMemoryRevocationStore` (development only) with a persistent implementation.
    /// The `InMemoryRevocationStore activated — DEV ONLY` warning disappears after this call.
    ///
    /// Used in `main.rs` when `config.auth.revocation_store == "sqlite"`.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
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

    /// Loads an ACL preset from `path` and injects it into the state (production wiring).
    ///
    /// The file at `path` must be an ACL preset TOML (`bearer.toml`).
    /// If the file is absent, unreadable, or the path is empty → fallback DENY-ALL
    /// (`AclEngine::from_preset_str("")`).
    ///
    /// Used in `main.rs` after reading `cfg.acl.preset_path`.
    ///
    /// # Example
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

    /// Creates a `JsonlFileSink` on `dir` and injects it as the audit sink (production wiring).
    ///
    /// The directory is created automatically on the first call to `record`.
    /// Replaces the default `NoopAuditSink`.
    ///
    /// Used in `main.rs` for optional production wiring.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use gradatum_server::state::AppState;
    /// // In an async Tokio context:
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

    /// Injects a pre-built embedder (production wiring).
    ///
    /// Replaces the default `Noop(384)` with an `HttpEmbedder` configured via `cfg.embed`.
    /// Used in `main.rs` when `cfg.embed.enabled` is true.
    ///
    /// Builder pattern: `state.with_embedder(Arc::new(HttpEmbedder::new(...)))`
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Replaces the default reranker (`NoopReranker`) with an alternative implementation.
    ///
    /// Used to wire `JinaOnnxReranker` when the `onnx-reranker` feature
    /// is enabled and an ONNX model file is available.
    ///
    /// Builder pattern: `state.with_reranker(Arc::new(JinaOnnxReranker::from_file(p)?))`
    #[allow(dead_code)]
    pub fn with_reranker(mut self, reranker: Arc<dyn gradatum_search::Reranker>) -> Self {
        self.reranker = reranker;
        self
    }

    /// Injects a [`QueueStore`] and its SQLite pool for job endpoints.
    ///
    /// Builder pattern: `state.with_job_store(Arc::new(SqliteQueueStore::new(pool.clone())), pool)`
    ///
    /// In production, called from `main.rs` after initialising the WAL pool
    /// on `cfg.storage.root/db/queue.sqlite` (same file as the worker).
    /// The pool is required for idempotency operations (table `gradatum_idempotency`).
    pub fn with_job_store(mut self, store: Arc<dyn QueueStore>, pool: SqlitePool) -> Self {
        self.job_store = store;
        self.jobs_pool = Some(pool);
        self
    }

    /// Opens an `EventLogStore` at `path` and injects it into the state (production wiring).
    ///
    /// `path` must point to the same `index.db` file as `with_search_path`
    /// (migration 0006 adds the `event_log` table there).
    ///
    /// Opens a dedicated connection in WAL mode (safe for SQLite multi-connection).
    /// The store is used by `POST /api/v1/event-log` and the retention task.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite file is inaccessible.
    pub async fn with_event_log_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use crate::event_log_store::EventLogStore;
        let store = EventLogStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("EventLogStore init failed: {e}"))?;
        self.event_log = Some(store);
        Ok(self)
    }

    /// Injects a pre-built `EventLogStore` (for integration tests).
    ///
    /// Pattern: `state.with_event_log(EventLogStore::open_in_memory().await?)`
    #[cfg(test)]
    pub fn with_event_log(mut self, store: EventLogStore) -> Self {
        self.event_log = Some(store);
        self
    }

    /// Opens a `SessionTraceStore` at `path` and injects it into the state (production wiring).
    ///
    /// `path` must point to the same `index.db` file as `with_search_path`
    /// (migration 0015 adds the `session_trace` table there).
    ///
    /// Opens a dedicated connection in WAL mode (safe for SQLite multi-connection).
    /// The store is used by `POST /api/v1/session-log/trace` and the 90-day retention task.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite file is inaccessible.
    pub async fn with_session_trace_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        use crate::session_trace_store::SessionTraceStore;
        let store = SessionTraceStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("SessionTraceStore init failed: {e}"))?;
        self.session_trace = Some(store);
        Ok(self)
    }

    /// Injecte un `SessionTraceStore` pré-construit (dev/test uniquement).
    ///
    /// Pattern: `state.with_session_trace(SessionTraceStore::open_in_memory().await?)`
    ///
    /// **Ne pas utiliser en production** — utiliser [`AppState::with_session_trace_path`].
    /// Méthode disponible sans gate `#[cfg(test)]` pour permettre son usage depuis
    /// les tests d'intégration externes (`tests/`).
    #[allow(dead_code)]
    pub fn with_session_trace(
        mut self,
        store: crate::session_trace_store::SessionTraceStore,
    ) -> Self {
        self.session_trace = Some(store);
        self
    }

    /// Ouvre un `ProactiveSurfaceStore` à `path` et l'injecte dans l'état (câblage production).
    ///
    /// `path` doit pointer vers le même fichier `index.db` que `with_search_path`
    /// (la migration 0022 ajoute la table `proactive_surface`).
    ///
    /// Ouvre une connexion dédiée en mode WAL (safe multi-connexion SQLite).
    /// Used by the proactive refresh task and the recall handlers.
    ///
    /// # Errors
    ///
    /// Retourne une erreur si le fichier SQLite est inaccessible.
    pub async fn with_proactive_surface_path(
        mut self,
        path: &std::path::Path,
    ) -> anyhow::Result<Self> {
        use crate::proactive_surface_store::ProactiveSurfaceStore;
        let store = ProactiveSurfaceStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("ProactiveSurfaceStore init failed: {e}"))?;
        self.proactive_surface = Some(store);
        Ok(self)
    }

    /// Injecte un `ProactiveSurfaceStore` pré-construit (pour les tests).
    ///
    /// Pattern : `state.with_proactive_surface(ProactiveSurfaceStore::open_in_memory().await?)`
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_proactive_surface(
        mut self,
        store: crate::proactive_surface_store::ProactiveSurfaceStore,
    ) -> Self {
        self.proactive_surface = Some(store);
        self
    }

    /// Ouvre un `ProactiveRecallStore` à `path` et l'injecte dans l'état (câblage production).
    ///
    /// `path` doit pointer vers le même fichier `index.db` que `with_search_path`
    /// (la migration 0023 ajoute les tables `proactive_recall_sessions` et
    /// `proactive_recall_feedback`).
    ///
    /// Ouvre une connexion dédiée en mode WAL (safe multi-connexion SQLite).
    /// Used by the proactive recall feedback handlers.
    ///
    /// # Errors
    ///
    /// Retourne une erreur si le fichier SQLite est inaccessible.
    pub async fn with_proactive_recall_path(
        mut self,
        path: &std::path::Path,
    ) -> anyhow::Result<Self> {
        use crate::proactive_recall_store::ProactiveRecallStore;
        let store = ProactiveRecallStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("ProactiveRecallStore init failed: {e}"))?;
        self.proactive_recall = Some(store);
        Ok(self)
    }

    /// Injecte un `ProactiveRecallStore` pré-construit (dev/test uniquement).
    ///
    /// Pattern : `state.with_proactive_recall(ProactiveRecallStore::open_in_memory().await?)`
    ///
    /// **Ne pas utiliser en production** — utiliser [`AppState::with_proactive_recall_path`]
    /// qui ouvre le fichier depuis la config. Méthode disponible sans gate `cfg(test)` pour
    /// permettre son usage depuis les tests d'intégration externes (`tests/`).
    #[allow(dead_code)] // Utilisé dans tests/mcp_native.rs (test d'intégration externe).
    pub fn with_proactive_recall(
        mut self,
        store: crate::proactive_recall_store::ProactiveRecallStore,
    ) -> Self {
        self.proactive_recall = Some(store);
        self
    }

    /// Injects the trust-decay scoring configuration.
    ///
    /// Wired from `[scoring]` in the server config in production.
    #[must_use]
    pub fn with_scoring(mut self, scoring: gradatum_search::TrustDecayConfig) -> Self {
        self.scoring = Arc::new(scoring);
        self
    }

    /// Injecte la `ServerConfig` de production dans l'état.
    ///
    /// Permet à l'endpoint `GET /api/v1/system/scheduled` d'accéder aux intervalles
    /// réels des tâches via `task_interval_secs(name, &state.server_config)` — SSOT T4.
    /// Sans appel explicite, le défaut est `ServerConfig::default()` (valeurs de production
    /// par défaut). Wired via `main.rs` après le parsing du config.toml de production.
    #[must_use]
    pub fn with_server_config(mut self, cfg: crate::config::ServerConfig) -> Self {
        self.server_config = Arc::new(cfg);
        self
    }

    /// Injects the context assembly pipeline configuration.
    ///
    /// Wired depuis `[context]` dans la config serveur en production.
    /// En l'absence d'appel, les valeurs par défaut (`ContextConfig::default()`) s'appliquent.
    #[must_use]
    pub fn with_context(mut self, context: crate::config::ContextConfig) -> Self {
        self.context = Arc::new(context);
        self
    }

    /// Opens a `ReadUsageCounterStore` at `path` and injects it into the state (production wiring).
    ///
    /// `path` must point to the same `index.db` file as `with_search_path`
    /// (migration 0019 adds the `read_usage_counters` table there).
    ///
    /// Opens a dedicated connection in WAL mode (safe for SQLite multi-connection).
    /// The store is flushed every 60s by the flush task in `main.rs`.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQLite file is inaccessible.
    pub async fn with_read_usage_path(mut self, path: &std::path::Path) -> anyhow::Result<Self> {
        let store = ReadUsageCounterStore::open(path)
            .await
            .map_err(|e| anyhow::anyhow!("ReadUsageCounterStore init failed: {e}"))?;
        self.read_usage = Some(store);
        Ok(self)
    }

    /// Injecte le token pour l'API interne (production wiring).
    ///
    /// Appelé dans `main.rs` après lecture de `cfg.internal_api.token`.
    /// Si `token` est `None` (config par défaut) → le listener interne est désactivé.
    ///
    /// Builder pattern: `state.with_internal_api_token(token)`
    pub fn with_internal_api_token(mut self, token: secrecy::SecretString) -> Self {
        self.internal_api_token = Some(Arc::new(token));
        self
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
