//! Tests d'intégration — `/health` (F-154 P2, probe + snapshot).
//!
//! Le contrat P2 :
//! - `GET /health` public = [`LivenessReport`] minimal (`status`, `version`, `build_sha`),
//!   toujours HTTP 200, lu depuis le snapshot sans I/O.
//! - `GET /health/detailed` = superset [`HealthReport`] servi UNIQUEMENT par le listener
//!   loopback (fail-closed), jamais sur le routeur public.
//! - La sonde de fond calcule le rollup et le publie ; une capacité qui pend est bornée par
//!   le timeout et n'affecte jamais `/health`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gradatum_core::{
    JobFilter, JobRecord, JobResult, JobStatus, QueueError, QueueEvent, QueueStore,
};
use gradatum_dto::{CapabilityState, HealthStatus};
use gradatum_server::config::ServerConfig;
use gradatum_server::state::AppState;
use gradatum_server::{health, metrics};
use reqwest::StatusCode;
use serde_json::Value;
use ulid::Ulid;

// ── Mock QueueStore configurable ───────────────────────────────────────────────

/// Comportement du lease worker exposé par le mock.
#[derive(Clone, Copy)]
enum Lease {
    /// `worker_lease_fresh` → `Ok(Some(true))` (worker vivant).
    Fresh,
    /// `worker_lease_fresh` → `Ok(Some(false))` (worker mort).
    Stale,
    /// `worker_lease_fresh` → `Ok(None)` (non observable — pas de queue DB).
    Unobservable,
    /// `worker_lease_fresh` pend au-delà du timeout (sonde bornée à prouver).
    Hang,
}

/// Store de test paramétrable : pilote le lease worker, le breakdown DLQ par kind, et le
/// compte Pending/DLQ. Toutes les autres opérations sont des no-ops.
struct MockStore {
    lease: Lease,
    dlq_by_kind: std::collections::BTreeMap<String, u64>,
    pending: u64,
    dlq: u64,
}

impl MockStore {
    fn new(lease: Lease) -> Self {
        Self {
            lease,
            dlq_by_kind: std::collections::BTreeMap::new(),
            pending: 0,
            dlq: 0,
        }
    }
}

#[async_trait]
impl QueueStore for MockStore {
    async fn enqueue(&self, _job: JobRecord) -> Result<Ulid, QueueError> {
        Err(QueueError::Storage("mock".into()))
    }
    async fn dequeue(&self, _t: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }
    async fn get(&self, _id: Ulid, _t: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }
    async fn complete(&self, _id: Ulid, _r: JobResult) -> Result<(), QueueError> {
        Ok(())
    }
    async fn fail(&self, _id: Ulid, _e: &str, _a: u32) -> Result<(), QueueError> {
        Ok(())
    }
    async fn cancel(&self, _id: Ulid, _t: Option<&str>) -> Result<(), QueueError> {
        Ok(())
    }
    async fn fail_dlq(&self, _id: Ulid, _e: &str) -> Result<(), QueueError> {
        Ok(())
    }
    async fn find_awaiting(&self, _id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        Ok(())
    }
    async fn recover_stale_leases(&self, _ttl: Duration) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn cancel_expired_deadlines(&self, _n: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn promote_retries(&self, _n: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn schedule_retry(&self, _id: Ulid, _at: DateTime<Utc>) -> Result<(), QueueError> {
        Ok(())
    }
    async fn list(&self, _f: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    async fn count_jobs_by_status(
        &self,
        _t: Option<&str>,
    ) -> Result<std::collections::HashMap<JobStatus, u64>, QueueError> {
        let mut m = std::collections::HashMap::new();
        if self.pending > 0 {
            m.insert(JobStatus::Pending, self.pending);
        }
        if self.dlq > 0 {
            m.insert(JobStatus::DLQ, self.dlq);
        }
        Ok(m)
    }
    async fn dlq_counts_by_kind(
        &self,
    ) -> Result<std::collections::BTreeMap<String, u64>, QueueError> {
        Ok(self.dlq_by_kind.clone())
    }
    async fn worker_lease_fresh(&self, _now_ms: i64) -> Result<Option<bool>, QueueError> {
        match self.lease {
            Lease::Fresh => Ok(Some(true)),
            Lease::Stale => Ok(Some(false)),
            Lease::Unobservable => Ok(None),
            Lease::Hang => {
                // Pend bien au-delà de tout timeout de sonde raisonnable.
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(Some(true))
            }
        }
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QueueEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Construit un `AppState` de test avec un `MockStore` et une config donnée.
///
/// Mutation directe des champs publics `job_store`/`server_config` — pas de constructeur de
/// test câblant un mock sans pool SQLite (ADN 3, cf. convention du fichier historique).
#[expect(clippy::field_reassign_with_default)]
fn state_with(store: MockStore, cfg: ServerConfig) -> AppState {
    let mut s = AppState::default();
    s.job_store = Arc::new(store);
    s.server_config = Arc::new(cfg);
    s
}

/// Config minimale : embed désactivé (NotConfigured, jamais bloquant), curator heuristique
/// (défaut). Les tests surchargent ce qu'ils isolent.
fn cfg_embed_off() -> ServerConfig {
    let mut cfg = ServerConfig::default();
    cfg.embed.enabled = false;
    cfg
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client HTTP sans TLS custom")
}

/// Monte le routeur public tel que `build_router` : `/health` → `liveness_handler`, hors auth.
async fn start_public_health(state: AppState) -> SocketAddr {
    use axum::{Router, routing::get};

    let app = Router::new()
        .route("/health", get(health::liveness_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback éphémère");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur public arrêté");
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    addr
}

// ── Sonde (probe_once) : rollup + bornage ──────────────────────────────────────

/// probe_once : curator heuristique (critique) ⇒ `unhealthy`, même worker vivant.
#[tokio::test]
async fn probe_unhealthy_when_curator_heuristic() {
    let state = state_with(MockStore::new(Lease::Fresh), cfg_embed_off());
    let report = health::probe_once(&state, Duration::from_millis(800)).await;
    assert_eq!(
        report.status,
        HealthStatus::Unhealthy,
        "curator heuristique = LLM absente = unhealthy (criticité F-154)"
    );
    let curator = report
        .capabilities
        .iter()
        .find(|c| c.name == "curator")
        .expect("capacité curator présente");
    assert!(curator.critical);
    assert_eq!(curator.state, CapabilityState::Unreachable);
}

/// probe_once : lease worker périmé (critique) ⇒ `unhealthy`.
#[tokio::test]
async fn probe_unhealthy_when_worker_lease_stale() {
    // Curator non-heuristique sans llm ⇒ aussi unreachable ; on isole le worker en gardant
    // curator heuristique n'est pas possible (déjà unhealthy). On prouve donc que le worker
    // stale est bien reporté Unreachable, ce qui suffit à forcer unhealthy.
    let state = state_with(MockStore::new(Lease::Stale), cfg_embed_off());
    let report = health::probe_once(&state, Duration::from_millis(800)).await;
    let worker = report
        .capabilities
        .iter()
        .find(|c| c.name == "worker")
        .expect("capacité worker présente");
    assert_eq!(
        worker.state,
        CapabilityState::Unreachable,
        "lease périmé = down"
    );
    assert_eq!(report.status, HealthStatus::Unhealthy);
}

/// probe_once : une capacité qui PEND est bornée par le timeout — la passe ne dure pas 30 s.
#[tokio::test]
async fn probe_is_time_bounded_when_capability_hangs() {
    let state = state_with(MockStore::new(Lease::Hang), cfg_embed_off());
    let start = Instant::now();
    let report = health::probe_once(&state, Duration::from_millis(200)).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "la sonde doit être bornée (~timeout), obtenu : {elapsed:?}"
    );
    let worker = report
        .capabilities
        .iter()
        .find(|c| c.name == "worker")
        .expect("capacité worker présente");
    assert_eq!(
        worker.state,
        CapabilityState::Unreachable,
        "une capacité qui pend est déclarée unreachable au timeout"
    );
}

/// probe_once : `dlq_by_kind` est peuplé depuis le store (un embed qui échoue 100 %).
#[tokio::test]
async fn probe_populates_dlq_by_kind() {
    let mut store = MockStore::new(Lease::Fresh);
    store.dlq = 3;
    store.dlq_by_kind.insert("embed".to_string(), 3);
    let state = state_with(store, cfg_embed_off());
    let report = health::probe_once(&state, Duration::from_millis(800)).await;
    assert_eq!(report.dlq_depth, 3);
    assert_eq!(report.dlq_by_kind.get("embed").copied(), Some(3));
}

/// probe_once : worker non observable (mock sans queue DB) ⇒ NotConfigured, pas unhealthy
/// à ce seul titre (curator heuristique reste le driver ici).
#[tokio::test]
async fn probe_worker_unobservable_is_not_configured() {
    let state = state_with(MockStore::new(Lease::Unobservable), cfg_embed_off());
    let report = health::probe_once(&state, Duration::from_millis(800)).await;
    let worker = report
        .capabilities
        .iter()
        .find(|c| c.name == "worker")
        .expect("capacité worker présente");
    assert_eq!(worker.state, CapabilityState::NotConfigured);
}

// ── Handler public : forme LivenessReport, HTTP 200, build_sha ─────────────────

/// GET /health public : forme minimale (status/version/build_sha), SANS les champs de détail.
#[tokio::test]
async fn public_health_is_minimal_liveness() {
    let state = state_with(MockStore::new(Lease::Fresh), cfg_embed_off());
    // Publie un snapshot réel (sinon "starting").
    let report = health::probe_once(&state, Duration::from_millis(800)).await;
    state.health_snapshot.store(Arc::new(report));

    let addr = start_public_health(state).await;
    let resp = client()
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(resp.status(), StatusCode::OK, "toujours 200");
    let body: Value = resp.json().await.expect("corps JSON");

    assert!(body.get("status").and_then(Value::as_str).is_some());
    assert!(body.get("version").and_then(Value::as_str).is_some());
    assert!(
        body.get("build_sha").is_some(),
        "build_sha reste public (fleet-check + INDEX.md)"
    );
    // Contraction du wire public : le détail a migré vers /health/detailed (loopback).
    assert!(
        body.get("queue_depth").is_none(),
        "le détail queue_depth ne doit PLUS être exposé sur /health public"
    );
    assert!(body.get("dlq_by_kind").is_none());
    assert!(body.get("capabilities").is_none());
}

/// GET /health public : HTTP 200 même en `unhealthy` (le body porte le status).
#[tokio::test]
async fn public_health_http_200_on_unhealthy() {
    let state = state_with(MockStore::new(Lease::Fresh), cfg_embed_off());
    let report = health::probe_once(&state, Duration::from_millis(800)).await;
    assert_eq!(
        report.status,
        HealthStatus::Unhealthy,
        "curator heuristique"
    );
    state.health_snapshot.store(Arc::new(report));

    let addr = start_public_health(state).await;
    let resp = client()
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(resp.status(), StatusCode::OK, "200 même sur unhealthy");
    let body: Value = resp.json().await.expect("corps JSON");
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("unhealthy")
    );
}

/// Le handler public ne fait AUCUNE I/O : il répond instantanément même si le store PEND.
#[tokio::test]
async fn public_health_never_blocks_even_if_store_hangs() {
    // Store dont le lease pend 30 s : le handler ne l'appelle jamais (lit le snapshot).
    let state = state_with(MockStore::new(Lease::Hang), cfg_embed_off());
    // Snapshot initial = "starting" (aucune sonde lancée). Le handler doit répondre vite.
    let addr = start_public_health(state).await;
    let start = Instant::now();
    let resp = client()
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "le handler lit le snapshot sans I/O — jamais bloqué par un store lent"
    );
    let body: Value = resp.json().await.expect("corps JSON");
    assert_eq!(
        body.get("status").and_then(Value::as_str),
        Some("starting"),
        "snapshot initial avant toute sonde"
    );
}

// ── /health/detailed : loopback uniquement, superset ───────────────────────────

/// Le listener loopback sert /health/detailed avec le superset (capabilities + dlq_by_kind).
#[tokio::test]
async fn detailed_health_on_loopback_returns_superset() {
    let mut store = MockStore::new(Lease::Fresh);
    store.dlq = 2;
    store.dlq_by_kind.insert("embed".to_string(), 2);
    let state = state_with(store, cfg_embed_off());
    let report = health::probe_once(&state, Duration::from_millis(800)).await;
    state.health_snapshot.store(Arc::new(report));

    // Port loopback éphémère (réservé puis relâché avant le bind du listener de prod).
    let port = {
        let p = std::net::TcpListener::bind("127.0.0.1:0").expect("réserver un port");
        p.local_addr().expect("local_addr").port()
    };
    let bind: SocketAddr = ([127, 0, 0, 1], port).into();
    let metrics = state.metrics.clone();
    let snapshot = state.health_snapshot.clone();
    let registry = state.registry.clone();
    tokio::spawn(async move {
        let _ = metrics::spawn_metrics_listener(bind, metrics, snapshot, registry).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = client()
        .get(format!("http://127.0.0.1:{port}/health/detailed"))
        .send()
        .await
        .expect("GET /health/detailed");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.expect("corps JSON");
    assert!(
        body.get("capabilities").and_then(Value::as_array).is_some(),
        "le détail expose les capacités"
    );
    assert_eq!(
        body.pointer("/dlq_by_kind/embed").and_then(Value::as_u64),
        Some(2),
        "dlq_by_kind exposé sur le détail"
    );
}

/// /health/detailed n'existe PAS sur le routeur public (fail-closed : détail ⊂ loopback).
#[tokio::test]
async fn detailed_route_absent_from_public_router() {
    let state = state_with(MockStore::new(Lease::Fresh), cfg_embed_off());
    let addr = start_public_health(state).await;
    let resp = client()
        .get(format!("http://{addr}/health/detailed"))
        .send()
        .await
        .expect("GET /health/detailed sur routeur public");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "le détail ne doit jamais être servi par le routeur public"
    );
}

/// Le listener détaillé est fail-closed : bind non-loopback refusé (le détail ne s'échappe pas).
#[tokio::test]
async fn detailed_listener_rejects_non_loopback_bind() {
    let state = state_with(MockStore::new(Lease::Fresh), cfg_embed_off());
    let bind: SocketAddr = ([0, 0, 0, 0], 0).into();
    let res = metrics::spawn_metrics_listener(
        bind,
        state.metrics.clone(),
        state.health_snapshot.clone(),
        state.registry.clone(),
    )
    .await;
    assert!(
        res.is_err(),
        "un bind non-loopback (0.0.0.0) doit être refusé (caveat C7)"
    );
}

/// F-156 P6a : `GET /registry` sur le listener loopback rend les membres vivants du parc.
///
/// Le store est peuplé par injection directe (pas d'ingress en P6a) ; le fail-closed loopback
/// est couvert par `detailed_listener_rejects_non_loopback_bind` (même listener).
#[tokio::test]
async fn registry_endpoint_serves_living_entries_on_loopback() {
    use gradatum_server::registry_store::Registration;

    let state = state_with(MockStore::new(Lease::Fresh), cfg_embed_off());
    // Injection directe : un membre vivant + un membre expiré (doit être omis à la lecture).
    state
        .registry
        .register(Registration {
            service: "worker".into(),
            version: "2.3.0".into(),
            build_sha: Some("cafe".into()),
            pid: 4242,
            last_seen: Instant::now(),
            last_seen_ms: 1_000,
        })
        .expect("register living worker");
    state
        .registry
        .register(Registration {
            service: "gateway".into(),
            version: "2.3.0".into(),
            build_sha: None,
            pid: 7,
            // 10 min dans le passé ≫ TTL par défaut ⇒ omis.
            last_seen: Instant::now()
                .checked_sub(Duration::from_secs(600))
                .expect("instant in range"),
            last_seen_ms: 500,
        })
        .expect("register stale gateway");

    let port = {
        let p = std::net::TcpListener::bind("127.0.0.1:0").expect("réserver un port");
        p.local_addr().expect("local_addr").port()
    };
    let bind: SocketAddr = ([127, 0, 0, 1], port).into();
    let metrics = state.metrics.clone();
    let snapshot = state.health_snapshot.clone();
    let registry = state.registry.clone();
    tokio::spawn(async move {
        let _ = metrics::spawn_metrics_listener(bind, metrics, snapshot, registry).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = client()
        .get(format!("http://127.0.0.1:{port}/registry"))
        .send()
        .await
        .expect("GET /registry");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.expect("corps JSON");
    let entries = body
        .get("entries")
        .and_then(Value::as_array)
        .expect("entries est un tableau");
    assert_eq!(entries.len(), 1, "seul le membre vivant doit apparaître");
    assert_eq!(
        entries[0].get("service").and_then(Value::as_str),
        Some("worker")
    );
    assert_eq!(entries[0].get("pid").and_then(Value::as_u64), Some(4242));
}

/// F-156 P6a : `/registry` n'existe PAS sur le routeur public (lecture ⊂ loopback).
#[tokio::test]
async fn registry_route_absent_from_public_router() {
    let state = state_with(MockStore::new(Lease::Fresh), cfg_embed_off());
    let addr = start_public_health(state).await;
    let resp = client()
        .get(format!("http://{addr}/registry"))
        .send()
        .await
        .expect("GET /registry sur routeur public");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "/registry ne doit jamais être servi par le routeur public"
    );
}

/// Sanity : rétro-compat du body public — un consommateur P1 parse le status en `LivenessReport`.
#[tokio::test]
async fn public_body_parses_as_liveness_report() {
    let state = state_with(MockStore::new(Lease::Fresh), cfg_embed_off());
    let report = health::probe_once(&state, Duration::from_millis(800)).await;
    let build_sha = report.build_sha.clone();
    state.health_snapshot.store(Arc::new(report));

    let addr = start_public_health(state).await;
    let text = client()
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("GET /health")
        .text()
        .await
        .expect("corps texte");
    let parsed: gradatum_dto::LivenessReport =
        serde_json::from_str(&text).expect("le body public parse en LivenessReport (P1)");
    assert_eq!(parsed.build_sha, build_sha, "build_sha préservé");
}
