//! Tests d'intégration — GET /health (T10).
//!
//! Vérifie que le handler retourne un payload JSON complet avec les 12 champs
//! sans authentification (RFC-0003 §8 — endpoint unauthenticated).
//!
//! # Pattern de test
//!
//! Un serveur Axum est démarré sur un port éphémère avec `AppState::default()`.
//! `/health` est monté directement, sans middleware auth — identique à `build_router`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gradatum_core::{
    Job, JobClass, JobFilter, JobLifecycle, JobLineage, JobMode, JobOrder, JobPriority, JobRecord,
    JobResult, JobRetry, JobScheduling, JobSpec, JobStatus, QueueError, QueueEvent, QueueStore,
    RetryBackoff, TriggerSource,
};
use gradatum_server::state::AppState;
use reqwest::StatusCode;
use serde_json::Value;
use ulid::Ulid;

// ── Helper ────────────────────────────────────────────────────────────────────

/// Démarre un serveur de test minimaliste avec uniquement `/health`.
///
/// Reproduit le montage de `build_router` : `/health` hors middleware auth.
async fn start_health_server() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_server::{api_v1, health};

    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
        req.extensions_mut().insert(TrustContext::Unauthenticated);
        next.run(req).await
    }

    let state = AppState::default();
    let app = Router::new()
        // /health monté avant le layer middleware — pas d'auth requise.
        .route("/health", get(health::handler))
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("obtenir l'adresse locale — listener actif");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur de test health arrêté proprement");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Client reqwest sans retry, timeout 5s.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("construction client HTTP — pas de TLS custom")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// GET /health — 200 OK sans authentification.
#[tokio::test]
async fn health_no_auth_required() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/health doit retourner 200 sans bearer (unauthenticated RFC-0003 §8)"
    );
}

/// GET /health — payload JSON avec les 12 champs présents et typés correctement.
#[tokio::test]
async fn health_returns_full_payload() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");

    assert_eq!(resp.status(), StatusCode::OK, "/health doit retourner 200");

    let body: Value = resp.json().await.expect("corps JSON valide depuis /health");

    // ── Champ 1 : status ──────────────────────────────────────────────────────
    let status = body
        .get("status")
        .expect("champ 'status' présent dans /health");
    assert!(
        status.is_string(),
        "'status' doit être une string, obtenu : {status}"
    );
    let status_str = status.as_str().unwrap();
    assert!(
        status_str == "ok" || status_str == "degraded",
        "'status' doit valoir \"ok\" ou \"degraded\", obtenu : \"{status_str}\""
    );

    // ── Champ 2 : version ─────────────────────────────────────────────────────
    let version = body
        .get("version")
        .expect("champ 'version' présent dans /health");
    assert!(version.is_string(), "'version' doit être une string");
    assert!(
        !version.as_str().unwrap().is_empty(),
        "'version' ne doit pas être vide"
    );

    // ── Champ 3 : build_sha ───────────────────────────────────────────────────
    let build_sha = body
        .get("build_sha")
        .expect("champ 'build_sha' présent dans /health");
    assert!(build_sha.is_string(), "'build_sha' doit être une string");
    assert!(
        !build_sha.as_str().unwrap().is_empty(),
        "'build_sha' ne doit pas être vide"
    );

    // ── Champ 4 : uptime_secs ─────────────────────────────────────────────────
    let uptime = body
        .get("uptime_secs")
        .expect("champ 'uptime_secs' présent dans /health");
    assert!(
        uptime.is_u64() || uptime.is_number(),
        "'uptime_secs' doit être un nombre entier non-négatif, obtenu : {uptime}"
    );
    // Doit être >= 0 (u64 implicitement). Borne haute raisonnable : < 3600s pour un test.
    let uptime_val = uptime.as_u64().expect("'uptime_secs' convertible en u64");
    assert!(
        uptime_val < 3600,
        "'uptime_secs' trop grand pour un test (>= 3600s) : {uptime_val}"
    );

    // ── Champ 5 : tenant_count ────────────────────────────────────────────────
    let tenant_count = body
        .get("tenant_count")
        .expect("champ 'tenant_count' présent dans /health");
    assert!(
        tenant_count.is_u64() || tenant_count.is_number(),
        "'tenant_count' doit être un entier non-négatif"
    );
    // Stub T10 : 0 attendu.
    assert_eq!(
        tenant_count.as_u64().unwrap_or(u64::MAX),
        0,
        "'tenant_count' doit être 0 (stub T10)"
    );

    // ── Champ 6 : locus_count ─────────────────────────────────────────────────
    let locus_count = body
        .get("locus_count")
        .expect("champ 'locus_count' présent dans /health");
    assert!(
        locus_count.is_u64() || locus_count.is_number(),
        "'locus_count' doit être un entier non-négatif"
    );
    // Stub T10 : 0 attendu.
    assert_eq!(
        locus_count.as_u64().unwrap_or(u64::MAX),
        0,
        "'locus_count' doit être 0 (stub T10)"
    );

    // ── Champ 7 : queue_depth ─────────────────────────────────────────────────
    let queue_depth = body
        .get("queue_depth")
        .expect("champ 'queue_depth' présent dans /health");
    assert!(
        queue_depth.is_u64() || queue_depth.is_number(),
        "'queue_depth' doit être un entier non-négatif"
    );

    // ── Champ 8 : queue_oldest_age_secs ──────────────────────────────────────
    let queue_oldest = body
        .get("queue_oldest_age_secs")
        .expect("champ 'queue_oldest_age_secs' présent dans /health");
    assert!(
        queue_oldest.is_u64() || queue_oldest.is_number(),
        "'queue_oldest_age_secs' doit être un entier non-négatif"
    );

    // ── Champs F-204/F-206 : dlq_depth + dlq_oldest_age_secs ──────────────────
    let dlq_depth = body
        .get("dlq_depth")
        .expect("champ 'dlq_depth' présent dans /health");
    assert!(
        dlq_depth.is_u64() || dlq_depth.is_number(),
        "'dlq_depth' doit être un entier non-négatif"
    );
    let dlq_oldest = body
        .get("dlq_oldest_age_secs")
        .expect("champ 'dlq_oldest_age_secs' présent dans /health");
    assert!(
        dlq_oldest.is_u64() || dlq_oldest.is_number(),
        "'dlq_oldest_age_secs' doit être un entier non-négatif"
    );

    // ── Champ 9 : sqlite_wal_size_bytes ───────────────────────────────────────
    let wal_size = body
        .get("sqlite_wal_size_bytes")
        .expect("champ 'sqlite_wal_size_bytes' présent dans /health");
    assert!(
        wal_size.is_u64() || wal_size.is_number(),
        "'sqlite_wal_size_bytes' doit être un entier non-négatif"
    );

    // ── Champ 10 : started_at ─────────────────────────────────────────────────
    let started_at = body
        .get("started_at")
        .expect("champ 'started_at' présent dans /health");
    assert!(started_at.is_string(), "'started_at' doit être une string");
    let started_at_str = started_at.as_str().unwrap();
    // Validation format RFC3339 minimal : doit contenir 'T' et '+' ou 'Z'.
    assert!(
        started_at_str.contains('T'),
        "'started_at' ne ressemble pas à du RFC3339 (pas de 'T') : \"{started_at_str}\""
    );
    assert!(
        started_at_str.contains('+') || started_at_str.ends_with('Z'),
        "'started_at' ne ressemble pas à du RFC3339 (pas de timezone) : \"{started_at_str}\""
    );
}

/// GET /health — status "ok" quand les stubs retournent 0 (queue vide).
#[tokio::test]
async fn health_status_ok_with_stub_zeros() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");

    let body: Value = resp.json().await.expect("corps JSON valide");
    let status = body["status"].as_str().expect("'status' string");
    assert_eq!(
        status, "ok",
        "status doit être \"ok\" quand queue_depth=0 et queue_oldest_age_secs=0"
    );
}

/// GET /health — Content-Type application/json.
#[tokio::test]
async fn health_content_type_json() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("Content-Type header présent")
        .to_str()
        .expect("Content-Type valide UTF-8");
    assert!(
        content_type.contains("application/json"),
        "Content-Type doit contenir 'application/json', obtenu : \"{content_type}\""
    );
}

// ── B2 : build_sha non-placeholder ──────────────────────────────────────────

/// GET /health — `build_sha` expose un vrai SHA git, pas le placeholder "unknown".
///
/// Valide que le `build.rs` a bien injecté `BUILD_SHA` au compile-time.
/// Si cette env var n'est pas settée (build.rs absent ou git absent), le handler
/// retourne "unknown" — ce test échoue intentionnellement AVANT l'implémentation.
#[tokio::test]
async fn health_build_sha_is_not_unknown_placeholder() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");

    let body: Value = resp.json().await.expect("corps JSON valide depuis /health");

    let build_sha = body
        .get("build_sha")
        .expect("champ 'build_sha' présent")
        .as_str()
        .expect("'build_sha' est une string");

    assert_ne!(
        build_sha, "unknown",
        "`build_sha` ne doit pas être le placeholder \"unknown\" — \
         vérifier que build.rs émet BUILD_SHA=<git-sha> au compile-time"
    );
    assert!(!build_sha.is_empty(), "`build_sha` ne doit pas être vide");
    // SHA court git : typiquement 7-12 hex chars — validation format minimaliste.
    assert!(
        build_sha.len() >= 6 && build_sha.len() <= 16,
        "`build_sha` doit ressembler à un SHA git court (6-16 chars), obtenu : \"{build_sha}\""
    );
    assert!(
        build_sha.chars().all(|c| c.is_ascii_hexdigit()),
        "`build_sha` doit être uniquement des caractères hex ASCII, obtenu : \"{build_sha}\""
    );
}

// ── B1 : queue_oldest_age_secs réel ──────────────────────────────────────────

/// Mock `QueueStore` qui simule un job Pending créé il y a 600 secondes.
///
/// DT-OBS-1 : `queue_oldest_age_secs` est calculé depuis `job_store`
/// (trait `QueueStore`, table `gradatum_jobs`). L'ancien second bras
/// (`AppState.queue`, trait `Queue` legacy, table `jobs_v2`) est supprimé en
/// 2.1.0 (F-177). Ce mock valide le câblage du handler `/health` sur la source unifiée.
struct SlowJobStore;

impl SlowJobStore {
    /// Construit un `JobRecord` factice avec `lifecycle.created_at` ancré 600s dans le passé.
    ///
    /// Le handler `/health` lit `lifecycle.created_at` depuis le premier résultat
    /// de `list(Pending, CreatedAsc, 1)` pour calculer l'âge.
    fn make_old_pending_job() -> JobRecord {
        let old_created_at = Utc::now() - chrono::Duration::seconds(600);
        let now = Utc::now();
        JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Backup,
                class: JobClass::System,
                mode: JobMode::Batch,
                scope: gradatum_core::job::JobScope::VaultWide,
                priority: JobPriority::default_for(&JobClass::System),
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Pending,
                // Ancré 600s dans le passé pour dépasser le seuil "degraded" (300s).
                created_at: old_created_at,
                started_at: None,
                completed_at: None,
                lease_until: None,
                result: None,
            },
            retry: JobRetry {
                count: 0,
                max: 3,
                backoff: RetryBackoff::Exponential { base: 5, max: 120 },
                last_error: None,
                errors: vec![],
            },
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        }
    }
}

#[async_trait]
impl QueueStore for SlowJobStore {
    async fn enqueue(&self, _job: JobRecord) -> Result<Ulid, QueueError> {
        Err(QueueError::Storage("SlowJobStore stub".into()))
    }

    async fn dequeue(&self, _tenant_filter: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }

    async fn get(&self, _id: Ulid, _tenant: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }

    async fn complete(&self, _id: Ulid, _result: JobResult) -> Result<(), QueueError> {
        Ok(())
    }

    async fn fail(&self, _id: Ulid, _err: &str, _attempt: u32) -> Result<(), QueueError> {
        Ok(())
    }

    async fn cancel(&self, _id: Ulid, _tenant: Option<&str>) -> Result<(), QueueError> {
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

    async fn recover_stale_leases(
        &self,
        _ttl: std::time::Duration,
    ) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }

    async fn cancel_expired_deadlines(&self, _now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }

    async fn promote_retries(&self, _now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }

    async fn schedule_retry(&self, _id: Ulid, _at: DateTime<Utc>) -> Result<(), QueueError> {
        Ok(())
    }

    /// Retourne un job Pending créé il y a 600s — valide le calcul d'âge dans `/health`.
    async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        // Simule un store avec 1 job Pending ancien uniquement si le filtre le demande.
        if filter.status == Some(JobStatus::Pending) && filter.order == JobOrder::CreatedAsc {
            Ok(vec![Self::make_old_pending_job()])
        } else {
            Ok(vec![])
        }
    }

    /// Retourne 1 job Pending pour que `queue_depth > 0`.
    async fn count_jobs_by_status(
        &self,
        _tenant_filter: Option<&str>,
    ) -> Result<std::collections::HashMap<JobStatus, u64>, QueueError> {
        let mut m = std::collections::HashMap::new();
        m.insert(JobStatus::Pending, 1u64);
        Ok(m)
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QueueEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

/// Démarre un serveur de test avec un job_store simulant un job ancien (600s).
///
/// DT-OBS-1 : injecte `SlowJobStore` via `with_job_store` (source unifiée).
async fn start_health_server_with_slow_queue() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_server::{api_v1, health};

    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
        req.extensions_mut().insert(TrustContext::Unauthenticated);
        next.run(req).await
    }

    // DT-OBS-1 : injecte `SlowJobStore` sur le champ public `job_store`.
    // `with_job_store` requiert un `SqlitePool` (idempotency) non nécessaire ici.
    // `#[expect(clippy::field_reassign_with_default)]` : pas de constructeur de test
    // sans `SqlitePool` disponible — la mutation directe est la solution la plus
    // économique (ADN 3) sans ajouter de builder `with_job_store_no_pool`.
    #[expect(clippy::field_reassign_with_default)]
    let state = {
        let mut s = AppState::default();
        s.job_store = Arc::new(SlowJobStore);
        s
    };
    let app = Router::new()
        .route("/health", get(health::handler))
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("obtenir l'adresse locale — listener actif");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur de test health slow_queue arrêté proprement");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// GET /health — `queue_oldest_age_secs` reflète la vraie ancienneté du job le plus vieux.
///
/// Avec `SlowQueue` (oldest = 600s), le champ doit être > 0 ET le status doit
/// passer à "degraded" (seuil 300s).
#[tokio::test]
async fn health_queue_oldest_age_secs_reflects_real_age() {
    let addr = start_health_server_with_slow_queue().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health avec slow queue");

    assert_eq!(resp.status(), StatusCode::OK, "/health doit retourner 200");

    let body: Value = resp.json().await.expect("corps JSON valide depuis /health");

    let age = body
        .get("queue_oldest_age_secs")
        .expect("champ 'queue_oldest_age_secs' présent")
        .as_u64()
        .expect("'queue_oldest_age_secs' convertible en u64");

    assert!(
        age > 0,
        "`queue_oldest_age_secs` doit être > 0 quand un job attend depuis 600s, obtenu : {age}"
    );

    // Bonus : le status doit passer à "degraded" (oldest_age_secs=600 > seuil=300).
    let status = body["status"].as_str().expect("'status' string");
    assert_eq!(
        status, "degraded",
        "status doit être \"degraded\" quand queue_oldest_age_secs=600 > 300"
    );
}

// ── F-204 / F-206 : signal DLQ (compte + ancienneté) ──────────────────────────

/// Store simulant une DLQ non vide : 1 job mort, `created_at` ancré 48 h dans le passé.
///
/// Reproduit le défaut F-204/F-206 : un travail dont les tentatives sont épuisées
/// gît en DLQ sans que rien ne le signale. Le seuil `/health` (`DLQ_MAX_AGE_SECS = 24 h`)
/// doit être dépassé → `degraded`, et `dlq_depth` doit compter le job.
struct DlqJobStore;

impl DlqJobStore {
    /// `JobRecord` en statut `DLQ`, `lifecycle.created_at` ancré 48 h dans le passé
    /// (> seuil 24 h) pour valider le passage en `degraded` sur l'ANCIENNETÉ.
    fn make_old_dlq_job() -> JobRecord {
        let old_created_at = Utc::now() - chrono::Duration::hours(48);
        let now = Utc::now();
        JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Backup,
                class: JobClass::System,
                mode: JobMode::Batch,
                scope: gradatum_core::job::JobScope::VaultWide,
                priority: JobPriority::default_for(&JobClass::System),
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::DLQ,
                created_at: old_created_at,
                started_at: None,
                completed_at: Some(now),
                lease_until: None,
                result: None,
            },
            retry: JobRetry {
                count: 3,
                max: 3,
                backoff: RetryBackoff::Exponential { base: 5, max: 120 },
                last_error: Some("max_retries atteint (3 / 3)".to_string()),
                errors: vec![],
            },
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        }
    }
}

#[async_trait]
impl QueueStore for DlqJobStore {
    async fn enqueue(&self, _job: JobRecord) -> Result<Ulid, QueueError> {
        Err(QueueError::Storage("DlqJobStore stub".into()))
    }

    async fn dequeue(&self, _tenant_filter: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }

    async fn get(&self, _id: Ulid, _tenant: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }

    async fn complete(&self, _id: Ulid, _result: JobResult) -> Result<(), QueueError> {
        Ok(())
    }

    async fn fail(&self, _id: Ulid, _err: &str, _attempt: u32) -> Result<(), QueueError> {
        Ok(())
    }

    async fn cancel(&self, _id: Ulid, _tenant: Option<&str>) -> Result<(), QueueError> {
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

    async fn recover_stale_leases(
        &self,
        _ttl: std::time::Duration,
    ) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }

    async fn cancel_expired_deadlines(&self, _now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }

    async fn promote_retries(&self, _now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }

    async fn schedule_retry(&self, _id: Ulid, _at: DateTime<Utc>) -> Result<(), QueueError> {
        Ok(())
    }

    /// Retourne le job DLQ ancien uniquement pour `list(DLQ, CreatedAsc)`.
    async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        if filter.status == Some(JobStatus::DLQ) && filter.order == JobOrder::CreatedAsc {
            Ok(vec![Self::make_old_dlq_job()])
        } else {
            Ok(vec![])
        }
    }

    /// 1 job en DLQ (aucun Pending) — `dlq_depth = 1`, `queue_depth = 0`.
    async fn count_jobs_by_status(
        &self,
        _tenant_filter: Option<&str>,
    ) -> Result<std::collections::HashMap<JobStatus, u64>, QueueError> {
        let mut m = std::collections::HashMap::new();
        m.insert(JobStatus::DLQ, 1u64);
        Ok(m)
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QueueEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

/// Démarre un serveur de test avec `DlqJobStore` injecté sur `job_store`.
async fn start_health_server_with_dlq() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_server::{api_v1, health};

    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
        req.extensions_mut().insert(TrustContext::Unauthenticated);
        next.run(req).await
    }

    // Même contrainte que `start_health_server_with_slow_queue` : pas de constructeur de
    // test sans `SqlitePool`, la mutation directe reste la plus économique (ADN 3).
    #[expect(clippy::field_reassign_with_default)]
    let state = {
        let mut s = AppState::default();
        s.job_store = Arc::new(DlqJobStore);
        s
    };
    let app = Router::new()
        .route("/health", get(health::handler))
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("obtenir l'adresse locale — listener actif");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur de test /health (DLQ) doit tourner");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// GET /health — un travail tombé en DLQ est COMPTABILISÉ (`dlq_depth`) et VISIBLE,
/// et son ancienneté (48 h > seuil 24 h) fait passer le status à `degraded`.
///
/// Couvre F-204/F-206 critère : « un travail tombé en DLQ est comptabilisé et visible
/// par la surface choisie » + « le signal se déclenche sur l'ancienneté ».
#[tokio::test]
async fn health_dlq_aged_job_is_counted_and_degrades() {
    let addr = start_health_server_with_dlq().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health avec DLQ");

    assert_eq!(resp.status(), StatusCode::OK, "/health doit retourner 200");

    let body: Value = resp.json().await.expect("corps JSON valide depuis /health");

    // Le compte DLQ est visible.
    let dlq_depth = body
        .get("dlq_depth")
        .expect("champ 'dlq_depth' présent")
        .as_u64()
        .expect("'dlq_depth' convertible en u64");
    assert_eq!(
        dlq_depth, 1,
        "`dlq_depth` doit compter le job mort, obtenu : {dlq_depth}"
    );

    // L'ancienneté reflète les 48 h.
    let dlq_age = body
        .get("dlq_oldest_age_secs")
        .expect("champ 'dlq_oldest_age_secs' présent")
        .as_u64()
        .expect("'dlq_oldest_age_secs' convertible en u64");
    assert!(
        dlq_age > 24 * 60 * 60,
        "`dlq_oldest_age_secs` doit dépasser 24 h pour un job mort depuis 48 h, obtenu : {dlq_age}"
    );

    // C'est l'ancienneté DLQ (et non le Pending, ici absent) qui déclenche `degraded`.
    let status = body["status"].as_str().expect("'status' string");
    assert_eq!(
        status, "degraded",
        "status doit être \"degraded\" quand un job DLQ dépasse le seuil d'ancienneté"
    );
}

/// GET /health — une DLQ vide n'altère rien : `dlq_depth = 0` et status `ok`.
///
/// Garde anti-faux-positif : le seuil d'ancienneté (strictement positif) ne se
/// déclenche jamais sur `dlq_oldest_age_secs = 0`.
#[tokio::test]
async fn health_empty_dlq_stays_ok() {
    // AppState::default() câble un NoopQueueStore → toutes les méthodes renvoient vide.
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");

    let body: Value = resp.json().await.expect("corps JSON valide depuis /health");

    assert_eq!(
        body.get("dlq_depth").and_then(Value::as_u64),
        Some(0),
        "`dlq_depth` doit être 0 sans DLQ"
    );
    assert_eq!(
        body.get("dlq_oldest_age_secs").and_then(Value::as_u64),
        Some(0),
        "`dlq_oldest_age_secs` doit être 0 sans DLQ"
    );
    assert_eq!(
        body["status"].as_str(),
        Some("ok"),
        "status doit rester \"ok\" quand la DLQ est vide"
    );
}
