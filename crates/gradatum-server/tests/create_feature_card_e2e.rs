//! E2E — `POST /api/v1/project-map/create-feature` (numéro attribué par le serveur).
//!
//! **Vrai chemin, aucun mock** : HTTP → middleware auth → handler → `create_feature_card_impl`
//! → allocation atomique (vrai `SqliteIndex` par défaut) → enqueue (vrai `SqliteQueueStore`).
//!
//! La preuve centrale : on relit le record **réellement enqueué** (`CurateSpec.body`) et on
//! vérifie que le numéro attribué par le serveur est injecté dans le corps que le worker
//! écrira. La fidélité du worker (record → carte écrite) est couverte par `e2e_write.rs` ;
//! ici on prouve que le numéro atteint le payload d'écriture, et que le client ne peut ni le
//! fournir ni en choisir un.
//!
//! Invariant vérifié : « pas de carte sans numéro, pas deux cartes avec le même ».

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::QueueStore;
use gradatum_core::job::Job;
use gradatum_server::state::AppState;
use reqwest::StatusCode;
use ulid::Ulid;

const TEST_CONSUMER_SUB: &str = "test-write-consumer";

const TEST_ACL_PRESET: &str = r#"
[[consumer]]
identity = "test-write-consumer"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

/// Corps de carte-feature VALIDE : les 5 rôles non-feature, SANS `[[feature:…]]`.
const VALID_BODY: &str = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]] \
     [[release:planned]] [[version:gradatum/backlog]]\n\nDescription de la carte.";

/// Démarre un serveur de test et rend `(adresse, job_store)` — le `job_store` est conservé
/// pour inspecter le record réellement enqueué.
async fn start_server() -> (SocketAddr, Arc<dyn QueueStore>) {
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
    let inspect: Arc<dyn QueueStore> = job_store.clone();

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
    (addr, inspect)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client HTTP")
}

async fn post_create(addr: SocketAddr, body: &serde_json::Value) -> reqwest::Response {
    client()
        .post(format!("http://{addr}/api/v1/project-map/create-feature"))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(body)
        .send()
        .await
        .expect("requête create-feature")
}

/// Lit le corps réellement enqueué pour un `job_id` — extrait de `CurateSpec.body`.
async fn enqueued_body(job_store: &Arc<dyn QueueStore>, job_id: &str) -> (String, Option<String>) {
    let ulid = Ulid::from_string(job_id).expect("job_id ULID valide");
    let rec = job_store
        .get(ulid, None)
        .await
        .expect("get job")
        .expect("job présent dans le store");
    match rec.spec.kind {
        Job::Curate(spec) => (
            spec.body.expect("le curate spec porte le corps"),
            spec.section_hint,
        ),
        other => panic!("job attendu Curate, obtenu {other:?}"),
    }
}

// ── Test 1 : le serveur attribue le numéro et l'injecte dans le corps enqueué ──

#[tokio::test]
async fn create_feature_assigns_number_and_injects_it_into_enqueued_body() {
    let (addr, job_store) = start_server().await;

    let resp = post_create(
        addr,
        &serde_json::json!({ "title": "Ma carte", "body": VALID_BODY }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "create-feature valide → 202"
    );

    let json: serde_json::Value = resp.json().await.expect("réponse JSON");
    // Vault vide → première allocation = 1.
    assert_eq!(json["number"], 1, "numéro attribué = 1 (vault vide)");
    assert_eq!(json["feature"], "F-01");
    let job_id = json["job_id"].as_str().expect("job_id string");

    // Preuve du vrai chemin : le corps RÉELLEMENT enqueué porte le numéro injecté,
    // que le client n'a jamais fourni.
    let (body, section) = enqueued_body(&job_store, job_id).await;
    assert_eq!(section.as_deref(), Some("project-map"));
    assert!(
        body.contains("[[feature:F-01]]"),
        "le numéro attribué doit être injecté dans le corps enqueué : {body}"
    );
}

// ── Test 2 : deux créations rendent deux numéros distincts (vrai chemin) ───────

#[tokio::test]
async fn two_creates_yield_distinct_numbers_end_to_end() {
    let (addr, job_store) = start_server().await;

    let r1: serde_json::Value = post_create(
        addr,
        &serde_json::json!({ "title": "C1", "body": VALID_BODY }),
    )
    .await
    .json()
    .await
    .expect("json 1");
    let r2: serde_json::Value = post_create(
        addr,
        &serde_json::json!({ "title": "C2", "body": VALID_BODY }),
    )
    .await
    .json()
    .await
    .expect("json 2");

    assert_eq!(r1["number"], 1);
    assert_eq!(r2["number"], 2, "deux créations → deux numéros distincts");

    let (b1, _) = enqueued_body(&job_store, r1["job_id"].as_str().unwrap()).await;
    let (b2, _) = enqueued_body(&job_store, r2["job_id"].as_str().unwrap()).await;
    assert!(b1.contains("[[feature:F-01]]"));
    assert!(b2.contains("[[feature:F-02]]"));
}

// ── Test 3 : le client ne peut PAS fournir de numéro (rôle feature interdit) ───

#[tokio::test]
async fn create_feature_rejects_client_supplied_feature_role_without_burning() {
    let (addr, _job_store) = start_server().await;

    let body_with_feature = format!("{VALID_BODY} [[feature:F-99]]");
    let resp = post_create(
        addr,
        &serde_json::json!({ "title": "Triche", "body": body_with_feature }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "un corps portant déjà [[feature:…]] doit être refusé (le client ne choisit pas)"
    );

    // Aucun numéro brûlé (rejet AVANT allocation) : la création valide suivante rend 1.
    let ok: serde_json::Value = post_create(
        addr,
        &serde_json::json!({ "title": "OK", "body": VALID_BODY }),
    )
    .await
    .json()
    .await
    .expect("json ok");
    assert_eq!(
        ok["number"], 1,
        "le refus ne doit pas brûler de numéro (première allocation valide = 1)"
    );
}

// ── Test 4 : carte incomplète refusée sans brûler de numéro ────────────────────

#[tokio::test]
async fn create_feature_rejects_incomplete_card_without_burning() {
    let (addr, _job_store) = start_server().await;

    // Manque [[release:…]] et [[version:…]] → carte-feature invalide.
    let incomplete = "[[project:gradatum]] [[status:OPEN]] [[kind:FEATURE]]\n\nIncomplète.";
    let resp = post_create(
        addr,
        &serde_json::json!({ "title": "Incomplète", "body": incomplete }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "carte incomplète → 400"
    );

    // Pré-validation avant allocation → aucun numéro brûlé.
    let ok: serde_json::Value = post_create(
        addr,
        &serde_json::json!({ "title": "OK", "body": VALID_BODY }),
    )
    .await
    .json()
    .await
    .expect("json ok");
    assert_eq!(
        ok["number"], 1,
        "carte incomplète refusée ne brûle pas de numéro"
    );
}

// ── Test 5 : non authentifié → 401 ────────────────────────────────────────────

#[tokio::test]
async fn create_feature_unauthenticated_401() {
    let (addr, _job_store) = start_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/project-map/create-feature"))
        .json(&serde_json::json!({ "title": "X", "body": VALID_BODY }))
        .send()
        .await
        .expect("requête sans bearer");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
