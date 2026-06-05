//! Tests d'intégration — 3 handlers MCP write + 1 jobs poll (T3 P2.0b).
//!
//! Vérifie pour chaque handler :
//! - **401 UNAUTHORIZED** — pas de bearer (TrustContext::Unauthenticated).
//! - **202 ACCEPTED** — bearer valide + ACL Write autorisé → job enqueued.
//! - **200 OK** — GET /api/v1/jobs/<id> → statut JSON.
//!
//! # Setup serveur de test
//!
//! Le serveur est démarré sur un port éphémère (bind `127.0.0.1:0`) avec :
//! - `AppState::with_jwt_and_acl` — clé éphémère + preset ACL autorisant `test-consumer`
//!   avec `write_patterns = ["main/*", "main/main"]` pour le tenant `"main"`.
//! - Middleware `trust_stub` — extrait le bearer header et crée un `TrustContext::BearerToken`
//!   avec `sub = token` (identique au pattern des tests T8).
//! - Queue : `NoopQueue` (défaut) — `enqueue` retourne toujours `job_id = 1`.

use std::net::SocketAddr;
use std::time::Duration;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_server::state::AppState;
use reqwest::StatusCode;

// ── Constante consumer de test ────────────────────────────────────────────────

/// Sub du bearer utilisé dans les tests 202 + jobs poll.
///
/// Doit correspondre à l'`identity` du consumer dans `TEST_ACL_PRESET`.
const TEST_CONSUMER_SUB: &str = "test-write-consumer";

/// Preset ACL minimal autorisant `TEST_CONSUMER_SUB` à écrire sur le tenant `"main"`.
///
/// Locus write : `main/main` (format `{tenant_id}/main` des handlers write).
const TEST_ACL_PRESET: &str = r#"
[[consumer]]
identity = "test-write-consumer"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

// ── Helper : spawn serveur de test avec ACL Write ─────────────────────────────

/// Démarre un serveur Axum de test avec un preset ACL autorisant les writes.
///
/// Retourne l'adresse de bind éphémère.
async fn start_write_test_server() -> SocketAddr {
    use axum::{middleware, routing::get, Router};
    use gradatum_server::api_v1;

    // Middleware trust stub : extrait le bearer header → TrustContext.
    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
        let trust = if let Some(auth) = req.headers().get(axum::http::header::AUTHORIZATION) {
            if let Ok(val) = auth.to_str() {
                if let Some(token) = val.strip_prefix("Bearer ") {
                    if !token.is_empty() {
                        TrustContext::BearerToken {
                            kid: "test-kid".to_string(),
                            aud: "gradatum".to_string(),
                            sub: token.to_string(),
                            scopes: vec!["read".to_string(), "write".to_string()],
                            tenant_id: "main".to_string(),
                        }
                    } else {
                        TrustContext::Unauthenticated
                    }
                } else {
                    TrustContext::Unauthenticated
                }
            } else {
                TrustContext::Unauthenticated
            }
        } else {
            TrustContext::Unauthenticated
        };
        req.extensions_mut().insert(trust);
        next.run(req).await
    }

    use gradatum_db_sqlite::{run_migrations, SqliteQueueStore};
    use gradatum_queue::SqliteQueue;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_PRESET)
        .expect("preset ACL de test valide — invariant statique");

    // Injecter une SqliteQueue in-memory réelle — nécessaire depuis P2.1 Task 6 :
    // get_job interroge queue.get(id) et retourne 404 si le job n'existe pas
    // (PlaceholderQueue retourne toujours Ok(None) → 404 sur poll).
    let queue = Arc::new(
        SqliteQueue::in_memory()
            .await
            .expect("SqliteQueue::in_memory() — invariant test"),
    );

    // Phase 1.2 : vault_write utilise state.job_store (gradatum_jobs) — câbler un
    // SqliteQueueStore in-memory pour que les tests de write retournent 202 (pas 500).
    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("jobs pool in-memory — invariant test");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs — invariant test");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_queue(queue as Arc<dyn gradatum_queue::Queue>)
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool);

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
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
            .expect("serveur de test arrêté proprement");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Client reqwest sans retry, timeout 10s.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("construction client HTTP — pas de TLS custom")
}

// ── Test 1 : vault_write sans bearer → 401 ───────────────────────────────────

/// `POST /api/v1/vault_write` sans bearer → 401 UNAUTHORIZED.
#[tokio::test]
async fn vault_write_unauthenticated_401() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_write", addr))
        .json(&serde_json::json!({
            "title": "Test note",
            "body": "Contenu de test."
        }))
        .send()
        .await
        .expect("requête vault_write sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_write sans bearer doit retourner 401"
    );
}

// ── Test 2 : vault_write avec bearer autorisé → 202 ──────────────────────────

/// `POST /api/v1/vault_write` avec bearer autorisé → 202 ACCEPTED + JSON EnqueuedResponse.
#[tokio::test]
async fn vault_write_returns_202_accepted() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_write", addr))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "Note de test T3",
            "body": "Contenu markdown de la note.",
            "tags": ["test", "p2.0b"],
            "section_hint": "decisions"
        }))
        .send()
        .await
        .expect("requête vault_write avec bearer autorisé");

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "vault_write avec bearer autorisé doit retourner 202"
    );

    // Vérifier la structure du JSON de réponse.
    let body: serde_json::Value = resp
        .json()
        .await
        .expect("réponse vault_write doit être du JSON valide");

    assert!(
        body.get("job_id").is_some(),
        "réponse 202 doit contenir job_id"
    );
    assert_eq!(body["status"], "queued", "status doit être 'queued'");
    let poll_url = body["poll_url"]
        .as_str()
        .expect("poll_url doit être une string");
    assert!(
        poll_url.starts_with("/api/v1/jobs/"),
        "poll_url doit commencer par /api/v1/jobs/ — reçu: {poll_url}"
    );
}

// ── Test 3 : non-régression B2 sync_wait retrait ─────────────────────────────

/// Régression B2 — `vault_write` ne doit PAS retourner le header `X-Gradatum-Wait`
/// et ne doit PAS retourner 408 quand le header est absent.
///
/// Vérifie que le retrait du stub sync_wait est effectif :
/// - Réponse 202 Accepted (pas 408).
/// - Aucun header `x-gradatum-wait` dans la réponse.
#[tokio::test]
async fn vault_write_does_not_include_x_gradatum_wait_header() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_write", addr))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "Note sans sync",
            "body": "Pas de header X-Gradatum-Wait attendu dans la réponse."
        }))
        .send()
        .await
        .expect("requête vault_write");

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "vault_write doit retourner 202 (pas 408 stub retiré)"
    );
    assert!(
        !resp.headers().contains_key("x-gradatum-wait"),
        "header X-Gradatum-Wait fantôme ne doit pas apparaître dans la réponse"
    );
}

/// Régression B2 — `vault_write` avec header `X-Gradatum-Wait: true` doit
/// retourner 202 et ignorer le header (plus de stub 408).
#[tokio::test]
async fn vault_write_ignores_x_gradatum_wait_header() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_write", addr))
        .bearer_auth(TEST_CONSUMER_SUB)
        .header("X-Gradatum-Wait", "true")
        .json(&serde_json::json!({
            "title": "Note sync ignorée",
            "body": "Le header X-Gradatum-Wait est ignoré post-retrait du stub."
        }))
        .send()
        .await
        .expect("requête vault_write avec header ignoré");

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "vault_write avec X-Gradatum-Wait doit retourner 202 (header ignoré, pas 408)"
    );
}

// ── Test 4 : GET /api/v1/jobs/<id> → 200 + statut JSON ───────────────────────

/// `GET /api/v1/jobs/<id>` → 200 OK + JSON JobStatusResponse.
///
/// Vérifie la structure de la réponse de poll jobs.
/// En T3 (stub), retourne toujours `status: "pending"`.
#[tokio::test]
async fn jobs_poll_returns_status() {
    let addr = start_write_test_server().await;

    // D'abord enqueue un job pour obtenir un job_id réaliste.
    let write_resp = client()
        .post(format!("http://{}/api/v1/vault_write", addr))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "Note pour poll test",
            "body": "Test jobs poll endpoint."
        }))
        .send()
        .await
        .expect("enqueue job pour test poll");

    assert_eq!(write_resp.status(), StatusCode::ACCEPTED);

    let enqueued: serde_json::Value = write_resp
        .json()
        .await
        .expect("réponse enqueue doit être du JSON valide");
    // Phase 1.2 : vault_write retourne un ULID string (gradatum_jobs).
    let job_id = enqueued["job_id"]
        .as_str()
        .expect("job_id doit être une string ULID (Phase 1.2 bridge job_store)");
    let poll_url = enqueued["poll_url"]
        .as_str()
        .expect("poll_url doit être une string");

    // Poll via l'URL retournée — chemin /api/v1/jobs/v2/<ulid> (Phase 1.2).
    let poll_resp = client()
        .get(format!("http://{}{}", addr, poll_url))
        .send()
        .await
        .expect("requête GET poll_url depuis vault_write");

    assert_eq!(
        poll_resp.status(),
        StatusCode::OK,
        "GET poll_url doit retourner 200 — job_id={job_id}"
    );

    let status_body: serde_json::Value = poll_resp
        .json()
        .await
        .expect("réponse jobs poll doit être du JSON valide");

    // get_job_v2 retourne JobRecord JSON complet — id + spec + lifecycle + etc.
    assert!(
        status_body.get("id").is_some(),
        "réponse get_job_v2 doit contenir id (ULID du JobRecord)"
    );
    assert!(
        status_body.get("lifecycle").is_some(),
        "réponse get_job_v2 doit contenir lifecycle (inclut status)"
    );
    assert_eq!(
        status_body["id"].as_str().expect("id doit être une string"),
        job_id,
        "job_id dans la réponse doit correspondre à l'ID enqueued"
    );
}
