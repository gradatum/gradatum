//! Tests E2E — `GET /api/v1/system/scheduled` (v0.7.5 F-85 T5).
//!
//! Couvre :
//! 1. `get_scheduled_unauthenticated_is_401` — auth obligatoire.
//! 2. `get_scheduled_returns_200_with_8_tasks` — 8 tâches après seed, champs attendus.
//! 3. `get_scheduled_interval_secs_via_ssot` — interval_secs == task_interval_secs SSOT.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::index::Index;
use gradatum_core::scheduled_health::TaskOutcome;
use gradatum_embed::error::EmbedError;
use gradatum_embed::{EmbedBackend, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_server::config::ServerConfig;
use gradatum_server::scheduled_tasks::{ALL_SCHEDULED_TASKS, task_interval_secs};
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// ACL authorisant le consommateur de test (miroir dashboard.rs)
// ---------------------------------------------------------------------------

const TEST_ACL: &str = r#"
[[consumer]]
identity = "sched-tester"
read_patterns  = ["main/*", "main/dashboard", "reference/*"]
write_patterns = []
"#;

// ---------------------------------------------------------------------------
// Embedder noop (zéro I/O, zéro réseau)
// ---------------------------------------------------------------------------

struct NoopBackend;

#[async_trait]
impl Embedder for NoopBackend {
    fn embedder_id(&self) -> &str {
        "noop-sched"
    }
    fn dim(&self) -> u16 {
        8
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(vec![0.0f32; 8])
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.0f32; 8]).collect())
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

// ---------------------------------------------------------------------------
// Helper : construit le router de test + index en mémoire
// ---------------------------------------------------------------------------

async fn build_app() -> (axum::Router, AppState, Arc<SqliteIndex>) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL system");

    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test system"),
    );

    let mut state = AppState::with_jwt_and_acl(jwt, acl).with_embedder(Arc::new(NoopBackend));
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, state, idx)
}

// ---------------------------------------------------------------------------
// Helper : signe un JWT pour le consommateur de test
// ---------------------------------------------------------------------------

fn sign(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "sched-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT system")
}

// ---------------------------------------------------------------------------
// Helper : construit une requête GET /api/v1/system/scheduled
// ---------------------------------------------------------------------------

fn get_scheduled(token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .uri("/api/v1/system/scheduled")
        .method("GET");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Sans token → 401 (l'endpoint est derrière auth, même groupe que /dashboard).
#[tokio::test]
async fn get_scheduled_unauthenticated_is_401() {
    let (app, _state, _idx) = build_app().await;
    let resp = app.oneshot(get_scheduled(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Avec token valide + 8 tâches seedées → 200 avec tableau de 8 tâches.
///
/// Vérifie :
/// - HTTP 200
/// - `tasks` est un tableau de 8 éléments (ALL_SCHEDULED_TASKS)
/// - chaque tâche contient les 8 champs attendus
/// - `last_run_ms` = null (jamais tické), `run_count` = 0, `errors_24h` = 0
/// - `interval_secs` ≥ 60 (plancher garanti par task_interval_secs)
/// - tous les noms correspondent à ALL_SCHEDULED_TASKS
#[tokio::test]
async fn get_scheduled_returns_200_with_8_tasks() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Seed des 8 tâches (INSERT OR IGNORE — idempotent).
    for task in ALL_SCHEDULED_TASKS {
        idx.seed_scheduled_task(task)
            .await
            .expect("seed_scheduled_task");
    }

    let resp = app.oneshot(get_scheduled(Some(&token))).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "doit retourner 200");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("réponse JSON valide");

    let tasks = json["tasks"]
        .as_array()
        .expect("tasks doit être un tableau");
    assert_eq!(tasks.len(), 8, "8 tâches attendues");

    // Vérifier que les 7 noms canoniques sont tous présents.
    let names: Vec<&str> = tasks
        .iter()
        .map(|t| t["name"].as_str().expect("name doit être string"))
        .collect();
    for expected_name in ALL_SCHEDULED_TASKS {
        assert!(
            names.contains(&expected_name),
            "tâche {expected_name} absente de la réponse"
        );
    }

    // Vérifier la structure des champs sur chaque tâche.
    for task in tasks {
        let name = task["name"].as_str().unwrap();
        // last_run_ms = null (jamais tické après seed seul).
        assert!(
            task["last_run_ms"].is_null(),
            "tâche {name} : last_run_ms doit être null avant premier tick"
        );
        // run_count = 0
        assert_eq!(
            task["run_count"].as_i64(),
            Some(0),
            "tâche {name} : run_count doit être 0 avant premier tick"
        );
        // errors_24h = 0
        assert_eq!(
            task["errors_24h"].as_i64(),
            Some(0),
            "tâche {name} : errors_24h doit être 0 avant premier tick"
        );
        // interval_secs >= 60 (plancher SSOT garanti)
        let interval = task["interval_secs"].as_u64().unwrap_or(0);
        assert!(
            interval >= 60,
            "tâche {name} : interval_secs doit être >= 60 (plancher 60s), got {interval}"
        );
    }
}

/// Consommateur authentifié mais sans scope Read sur `main/dashboard` → 403 Forbidden.
///
/// Un JWT valide (signature ok) signé pour un consumer inconnu de l'ACL preset
/// franchit le middleware (`is_authenticated()` = true) mais l'ACL évalue `Deny`
/// → le handler retourne 403. Plan T5 exige 401 ET 403.
#[tokio::test]
async fn get_scheduled_no_acl_scope_is_403() {
    let (app, state, _idx) = build_app().await;

    // Consumer inconnu du preset TEST_ACL → AclEngine::evaluate retourne Deny.
    let no_dash_token = state
        .jwt
        .sign(
            "no-dashboard-access",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT no-dashboard-access");

    let resp = app
        .oneshot(get_scheduled(Some(&no_dash_token)))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "consumer authentifié sans droits ACL sur main/dashboard doit recevoir 403"
    );
}

/// `interval_secs` via SSOT `task_interval_secs` — cohérence entre handler et SSOT.
///
/// Vérifie que la valeur `interval_secs` renvoyée par l'endpoint pour chaque tâche
/// correspond exactement à ce que `task_interval_secs(name, &ServerConfig::default())`
/// retourne — garantie zéro divergence badges « en retard » / intervalles réels.
#[tokio::test]
async fn get_scheduled_interval_secs_via_ssot() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);
    let cfg = ServerConfig::default();

    // Seed des 8 tâches.
    for task in ALL_SCHEDULED_TASKS {
        idx.seed_scheduled_task(task)
            .await
            .expect("seed_scheduled_task");
    }

    let resp = app.oneshot(get_scheduled(Some(&token))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let tasks = json["tasks"].as_array().unwrap();

    for task in tasks {
        let name = task["name"].as_str().unwrap();
        let actual_interval = task["interval_secs"].as_u64().unwrap();
        let expected_interval = task_interval_secs(name, &cfg);
        assert_eq!(
            actual_interval, expected_interval,
            "tâche {name} : interval_secs={actual_interval} ≠ task_interval_secs={expected_interval}"
        );
    }
}

/// `last_error` contenant un chemin absolu est masqué en `[path]` dans la réponse API.
///
/// Prouve que le sanitizer côté handler filtre les informations d'infrastructure
/// (chemins FS, URLs internes) avant sérialisation JSON — la valeur en DB reste intacte.
/// Utilise la tâche canonique `telemetry-flush` (présente dans ALL_SCHEDULED_TASKS).
#[tokio::test]
async fn get_scheduled_last_error_path_is_sanitized() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    // Seed la tâche canonique puis enregistre une erreur avec chemin absolu.
    idx.seed_scheduled_task("telemetry-flush")
        .await
        .expect("seed telemetry-flush");
    idx.record_task_run(
        "telemetry-flush",
        TaskOutcome::Error,
        5,
        Some("connexion refused /var/run/gradatum/index.db: no such file"),
        1_000_000_000_i64,
    )
    .await
    .expect("record_task_run avec chemin absolu");

    let resp = app.oneshot(get_scheduled(Some(&token))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let tasks = json["tasks"].as_array().unwrap();
    let task = tasks
        .iter()
        .find(|t| t["name"].as_str() == Some("telemetry-flush"))
        .expect("telemetry-flush dans la réponse");

    let last_error = task["last_error"]
        .as_str()
        .expect("last_error doit être string");

    // Le chemin absolu ne doit PAS être exposé en clair.
    assert!(
        !last_error.contains("/var/run/gradatum/index.db"),
        "last_error ne doit pas exposer le chemin absolu, got: {last_error}"
    );
    // Le token générique [path] doit le remplacer.
    assert!(
        last_error.contains("[path]"),
        "last_error doit contenir [path] à la place du chemin, got: {last_error}"
    );
}

/// `last_error` contenant une URL interne est masquée en `[url]` dans la réponse API.
///
/// Utilise la tâche canonique `purge-event-log` (présente dans ALL_SCHEDULED_TASKS).
#[tokio::test]
async fn get_scheduled_last_error_url_is_sanitized() {
    let (app, state, idx) = build_app().await;
    let token = sign(&state);

    idx.seed_scheduled_task("purge-event-log")
        .await
        .expect("seed purge-event-log");
    idx.record_task_run(
        "purge-event-log",
        TaskOutcome::Error,
        3,
        Some("timeout connecting to https://internal.llm.host:8080/v1/embed"),
        2_000_000_000_i64,
    )
    .await
    .expect("record_task_run avec URL interne");

    let resp = app.oneshot(get_scheduled(Some(&token))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let tasks = json["tasks"].as_array().unwrap();
    let task = tasks
        .iter()
        .find(|t| t["name"].as_str() == Some("purge-event-log"))
        .expect("purge-event-log dans la réponse");

    let last_error = task["last_error"]
        .as_str()
        .expect("last_error doit être string");

    assert!(
        !last_error.contains("internal.llm.host"),
        "last_error ne doit pas exposer l'URL interne, got: {last_error}"
    );
    assert!(
        last_error.contains("[url]"),
        "last_error doit contenir [url] à la place de l'URL, got: {last_error}"
    );
}
