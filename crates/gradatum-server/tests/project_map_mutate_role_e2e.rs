//! E2E — `POST /api/v1/project-map/mutate-role` (F-153).
//!
//! **Vrai chemin, aucun mock** : HTTP → middleware auth → handler → `mutate_role_impl`.
//!
//! Ce fichier prouve le **contrat synchrone** de l'opération : l'ACL/tenant au niveau requête,
//! le rejet du lot vide, et surtout l'invariant central **« un verdict par carte, jamais
//! agrégé »** — un lot mêlant refus (rôle inconnu/identité) et carte absente rend N verdicts
//! distincts, dans l'ordre, chacun avec son `outcome`.
//!
//! Les chemins nécessitant une carte VIVANTE (queued, conflit de hash, octet-identité après
//! écriture) sont couverts ailleurs : l'octet-identité de la mutation par les tests unitaires
//! `gradatum-core::project_map::mutate_role_tests`, le compare-and-swap par
//! `gradatum-vault::write::write_if_match`, et la fidélité record→carte par `e2e_write.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::QueueStore;
use gradatum_server::state::AppState;
use reqwest::StatusCode;

const TEST_CONSUMER_SUB: &str = "test-write-consumer";

const TEST_ACL_PRESET: &str = r#"
[[consumer]]
identity = "test-write-consumer"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

async fn start_server() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
    use gradatum_server::api_v1;

    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
        let trust = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty())
            .map_or(TrustContext::Unauthenticated, |token| {
                TrustContext::BearerToken {
                    kid: "test-kid".to_string(),
                    aud: "gradatum".to_string(),
                    sub: token.into(),
                    scopes: vec!["read".to_string(), "write".to_string()],
                    tenant_id: "main".into(),
                    jti: None,
                }
            });
        req.extensions_mut().insert(trust);
        next.run(req).await
    }

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_PRESET).expect("preset ACL valide");

    let jobs_pool = QueueDb::open_in_memory()
        .await
        .expect("jobs pool in-memory");
    run_migrations(&jobs_pool).await.expect("migrations jobs");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_job_store(job_store as Arc<dyn QueueStore>, jobs_pool);

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind éphémère");
    let addr = listener.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serveur de test");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client HTTP")
}

async fn post_mutate(addr: SocketAddr, body: &serde_json::Value) -> reqwest::Response {
    client()
        .post(format!("http://{addr}/api/v1/project-map/mutate-role"))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(body)
        .send()
        .await
        .expect("requête mutate-role")
}

/// Lot vide → 400 (niveau requête, jamais un verdict par carte sur un lot inexistant).
#[tokio::test]
async fn empty_batch_is_rejected_400() {
    let addr = start_server().await;
    let resp = post_mutate(addr, &serde_json::json!({ "mutations": [] })).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Requête non authentifiée → 401 (l'ACL/auth est au niveau requête).
#[tokio::test]
async fn unauthenticated_is_rejected_401() {
    let addr = start_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/project-map/mutate-role"))
        .json(&serde_json::json!({
            "mutations": [{
                "note_id": ulid::Ulid::generate().to_string(),
                "role_type": "status",
                "new_value": "DONE",
                "expected_sha256": "0".repeat(64),
            }]
        }))
        .send()
        .await
        .expect("requête");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// L'invariant central : un lot mêlant rôle inconnu, rôle identité (feature) et carte absente
/// rend **trois verdicts distincts, dans l'ordre**, jamais un seul agrégé.
#[tokio::test]
async fn batch_returns_one_verdict_per_card_never_aggregated() {
    let addr = start_server().await;
    let live_id = ulid::Ulid::generate().to_string();

    let resp = post_mutate(
        addr,
        &serde_json::json!({
            "mutations": [
                { // (0) rôle inconnu → refused (avant toute lecture)
                    "note_id": ulid::Ulid::generate().to_string(),
                    "role_type": "supersedes",
                    "new_value": "F-9",
                    "expected_sha256": "0".repeat(64),
                },
                { // (1) rôle identité feature → refused explicite
                    "note_id": ulid::Ulid::generate().to_string(),
                    "role_type": "feature",
                    "new_value": "F-9",
                    "expected_sha256": "0".repeat(64),
                },
                { // (2) rôle valide mais carte absente → not_found
                    "note_id": live_id,
                    "role_type": "status",
                    "new_value": "DONE",
                    "expected_sha256": "0".repeat(64),
                },
            ]
        }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK, "réponse 200 avec verdicts");
    let json: serde_json::Value = resp.json().await.expect("JSON");
    let results = json["results"].as_array().expect("results est un tableau");
    assert_eq!(results.len(), 3, "un verdict par carte : {json}");

    // (0) rôle inconnu → refused.
    assert_eq!(results[0]["outcome"], "refused");
    assert!(
        results[0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("unknown role type")),
        "message nommant le rôle inconnu : {}",
        results[0]
    );

    // (1) feature = identité → refused, message d'identité.
    assert_eq!(results[1]["outcome"], "refused");
    assert!(
        results[1]["error"]
            .as_str()
            .is_some_and(|e| e.contains("identity")),
        "message d'identité immuable : {}",
        results[1]
    );

    // (2) carte absente → not_found (et NON un refus générique).
    assert_eq!(results[2]["outcome"], "not_found");
    assert_eq!(results[2]["note_id"], live_id);
}

/// Sur un rôle valide mais une carte absente, l'ordre du pipeline fait primer l'absence : la
/// carte est lue AVANT la validation de valeur et le verrou optimiste, donc une valeur
/// hors-taxonomie n'est jamais atteinte — le verdict est `not_found`, pas `refused`.
#[tokio::test]
async fn missing_card_short_circuits_to_not_found() {
    let addr = start_server().await;
    let resp = post_mutate(
        addr,
        &serde_json::json!({
            "mutations": [{
                "note_id": ulid::Ulid::generate().to_string(),
                "role_type": "status",
                "new_value": "PAS_UN_STATUT",
                "expected_sha256": "deadbeef".repeat(8),
            }]
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let json: serde_json::Value = resp.json().await.expect("JSON");
    assert_eq!(json["results"][0]["outcome"], "not_found");
}
