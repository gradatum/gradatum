//! E2E test — poll job status (caveat C-impl-2).
//!
//! # Objectif
//!
//! Valider le contrat du pattern async 202 de gradatum :
//! `vault_write → 202 + {job_id, poll_url}` → `GET /api/v1/jobs/<id> → JobStatusResponse`.
//!
//! # Périmètre P2.0c
//!
//! Le handler `GET /api/v1/jobs/:id` retourne actuellement `"pending"` pour tout ID
//! valide (stub documenté — Queue::get(id) non exposé dans le trait Queue).
//! Ce test valide :
//! 1. La structure du 202 Accepted (job_id, status="queued", poll_url correct).
//! 2. La disponibilité du endpoint poll (200 OK + JSON valide).
//! 3. La structure de la réponse poll (job_id, status, attempts, last_error).
//!
//! # Note Phase 2.1
//!
//! Phase 2.1 ajoutera `Queue::get_status(id)` dans le trait et câblera le handler
//! pour retourner le vrai statut (`"done"` + `result.note_id` après worker).
//! Ce test sera amendé à ce moment pour asserter `status = "done"`.
//!

use std::sync::Arc;
use std::time::Duration;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_db_sqlite::{run_migrations, SqliteQueueStore};
use gradatum_server::middleware::auth_middleware;
use gradatum_server::{api_v1, state::AppState};
use serde_json::Value;
use sqlx::sqlite::SqlitePoolOptions;

// ── Preset ACL — écriture autorisée pour test-writer ─────────────────────────

const TEST_ACL_WRITE: &str = r#"
[[consumer]]
identity = "test-writer"
read_patterns = ["**"]
write_patterns = ["**"]
"#;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Démarre un serveur de test avec write ACL activé.
///
/// Retourne l'adresse et un bearer JWT avec scope write.
async fn spawn_write_server() -> (std::net::SocketAddr, String) {
    use axum::{middleware, Router};
    use gradatum_queue::SqliteQueue;

    let jwt = JwtService::new_ephemeral();
    let bearer = jwt
        .sign(
            "test-writer",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signer un bearer write de test — clé éphémère valide");

    let acl = AclEngine::from_preset_str(TEST_ACL_WRITE)
        .expect("preset ACL write de test toujours valide — invariant statique");

    // Queue legacy jobs_v2 (classify/downgrade).
    let queue = SqliteQueue::in_memory()
        .await
        .expect("SqliteQueue::in_memory pour test E2E poll");

    // Phase 1.2 : vault_write bridge vers job_store (gradatum_jobs) — nécessaire pour 202.
    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("jobs pool in-memory — invariant test v1-parity");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs — invariant test v1-parity");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_queue(Arc::new(queue))
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool);

    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère E2E poll — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("adresse locale E2E poll — listener actif");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur de test E2E poll arrêté proprement");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, bearer)
}

/// Client HTTP sans retry, timeout 5s.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("construction client HTTP — pas de TLS custom")
}

// ── Tests E2E poll ─────────────────────────────────────────────────────────────

/// E2E : vault_write retourne 202 + job_id + poll_url correcte.
///
/// Valide la structure du 202 Accepted (caveat C-impl-2).
#[tokio::test]
async fn vault_write_returns_202_with_job_id_and_poll_url() {
    let (addr, bearer) = spawn_write_server().await;

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "title": "E2E poll — test note",
            "body": "Corps de la note pour le test E2E poll job status.",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("requête vault_write E2E — serveur démarré");

    assert_eq!(
        resp.status(),
        202,
        "vault_write doit retourner 202 Accepted (job enqueued)"
    );

    let body: Value = resp
        .json()
        .await
        .expect("vault_write 202 : body JSON parseable");

    // Phase 1.2 : job_id est un ULID string (bridge job_store gradatum_jobs).
    assert!(
        body.get("job_id").is_some(),
        "202 : champ 'job_id' absent — DTO EnqueuedResponseUlid Phase 1.2"
    );
    let job_id = body["job_id"]
        .as_str()
        .expect("job_id doit être une string ULID Phase 1.2");
    assert!(
        !job_id.is_empty(),
        "job_id ne doit pas être vide (ULID Phase 1.2)"
    );

    // status : "queued".
    assert_eq!(body["status"], "queued", "202 : status doit être 'queued'");

    // poll_url : format Phase 1.2 = /api/v1/jobs/{ulid}/v2
    assert!(
        body.get("poll_url").is_some(),
        "202 : champ 'poll_url' absent — DTO EnqueuedResponseUlid Phase 1.2"
    );
    let poll_url = body["poll_url"]
        .as_str()
        .expect("poll_url doit être une string");
    assert_eq!(
        poll_url,
        format!("/api/v1/jobs/{job_id}/v2"),
        "poll_url Phase 1.2 doit pointer vers /api/v1/jobs/<ulid>/v2"
    );
}

/// E2E : GET /api/v1/jobs/<id> retourne 200 + JobStatusResponse valide.
///
/// Valide que l'endpoint poll est joignable et retourne un JSON conforme
/// au DTO JobStatusResponse (job_id, status, attempts, last_error).
#[tokio::test]
async fn poll_endpoint_returns_200_with_valid_job_status_response() {
    let (addr, bearer) = spawn_write_server().await;
    let c = client();

    // Enqueue un job.
    let write_resp = c
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "title": "E2E poll — endpoint check",
            "body": "Test que GET /jobs/:id est joignable.",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_write pour le test poll endpoint");

    assert_eq!(write_resp.status(), 202);
    let write_body: Value = write_resp.json().await.expect("body 202 parseable");
    // Phase 1.2 : job_id est un ULID string.
    let job_id = write_body["job_id"]
        .as_str()
        .expect("job_id doit être une string ULID Phase 1.2");

    // Phase 1.2 : poll_url = /api/v1/jobs/{ulid}/v2 (route get_job_v2).
    let poll_url = write_body["poll_url"]
        .as_str()
        .expect("poll_url doit être présent Phase 1.2");
    assert!(
        poll_url.ends_with("/v2"),
        "poll_url Phase 1.2 doit se terminer par /v2, obtenu: {poll_url}"
    );

    // Poll le job via la route /v2.
    let poll_resp = c
        .get(format!("http://{addr}{poll_url}"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("GET <poll_url> — endpoint doit être joignable");

    assert_eq!(
        poll_resp.status(),
        200,
        "GET <poll_url> doit retourner 200 (Phase 1.2 get_job_v2)"
    );

    let status_body: Value = poll_resp
        .json()
        .await
        .expect("JobRecord doit être JSON parseable Phase 1.2");

    // Phase 1.2 : get_job_v2 retourne JobRecord JSON complet.
    assert_eq!(
        status_body["id"].as_str(),
        Some(job_id),
        "id dans la réponse poll doit correspondre au job_id du 202"
    );
    assert!(
        status_body.get("lifecycle").is_some(),
        "JobRecord : champ 'lifecycle' absent Phase 1.2"
    );
}

/// E2E : 3 vault_write parallèles → 3 job_id distincts.
///
/// Vérifie que l'atomic `INSERT...RETURNING` de SqliteQueue garantit
/// l'unicité des job_id même sous charge concurrente.
#[tokio::test]
async fn concurrent_vault_write_produces_distinct_job_ids() {
    let (addr, bearer) = spawn_write_server().await;

    let futs: Vec<_> = (0..3u32)
        .map(|i| {
            let bearer = bearer.clone();
            tokio::spawn(async move {
                client()
                    .post(format!("http://{addr}/api/v1/vault_write"))
                    .bearer_auth(&bearer)
                    .json(&serde_json::json!({
                        "title": format!("Concurrent write #{i}"),
                        "body": format!("Corps de la note concurrente numéro {i}."),
                        "tenant_id": "main"
                    }))
                    .send()
                    .await
                    .expect("vault_write concurrent")
            })
        })
        .collect();

    let mut job_ids = Vec::new();
    for fut in futs {
        let resp = fut.await.expect("join tokio task");
        assert_eq!(
            resp.status(),
            202,
            "concurrent vault_write doit retourner 202"
        );
        let body: Value = resp.json().await.expect("body 202 concurrent");
        // Phase 1.2 : job_id est un ULID string.
        let jid = body["job_id"]
            .as_str()
            .expect("job_id doit être une string ULID concurrent Phase 1.2")
            .to_owned();
        job_ids.push(jid);
    }

    // Tous les job_id doivent être distincts (ULID garantit l'unicité).
    let unique: std::collections::HashSet<_> = job_ids.iter().collect();
    assert_eq!(
        unique.len(),
        3,
        "3 vault_write concurrents doivent produire 3 ULID distincts (Phase 1.2): {job_ids:?}"
    );
}

/// E2E : vault_classify enqueue valide — 202 + job_id + poll_url.
#[tokio::test]
async fn vault_classify_returns_202_with_job_id() {
    let (addr, bearer) = spawn_write_server().await;

    let fake_note_id = ulid::Ulid::new().to_string();

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_classify"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "note_id": fake_note_id,
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_classify E2E");

    assert_eq!(
        resp.status(),
        202,
        "vault_classify doit retourner 202 Accepted"
    );

    let body: Value = resp.json().await.expect("vault_classify 202 body");
    assert!(body["job_id"].as_i64().is_some(), "job_id présent");
    assert_eq!(body["status"], "queued");
    let poll_url = body["poll_url"].as_str().expect("poll_url string");
    assert!(
        poll_url.starts_with("/api/v1/jobs/"),
        "poll_url doit commencer par /api/v1/jobs/"
    );
}

/// E2E : vault_downgrade synchrone — 404 pour une note inexistante.
///
/// Phase 2.1.2 alpha.9 : vault_downgrade est désormais **synchrone** (200/404)
/// et non plus async 202. L'handler opère directement sur l'index SQLite.
/// Une note inexistante retourne 404 Not Found (plus de job_id enqueued).
#[tokio::test]
async fn vault_downgrade_returns_202_with_job_id() {
    let (addr, _bearer) = spawn_write_server().await;

    // Note inexistante en DB — l'handler sync retourne 404 directement.
    let fake_note_id = ulid::Ulid::new().to_string();

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_downgrade"))
        .json(&serde_json::json!({
            "note_id": fake_note_id,
            "reason": "obsolète — remplacé par une version révisée",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_downgrade E2E");

    assert_eq!(
        resp.status(),
        404,
        "vault_downgrade sync : note inexistante → 404 Not Found (Phase 2.1.2 alpha.9 — sync remplace async 202)"
    );
}

/// E2E : GET /api/v1/jobs/<id> avec id inconnu retourne 404.
///
/// Valide que le handler retourne 404 Not Found (et non 200 stub "pending")
/// depuis le fix RT5 Phase 2.1 — `Queue::get` retourne `None` pour un id
/// qui n'a jamais été enqueued.
#[tokio::test]
async fn poll_unknown_job_id_returns_404() {
    let (addr, bearer) = spawn_write_server().await;

    // ID arbitraire très grand — jamais enqueued dans cette instance in-memory.
    let unknown_id: i64 = 999_999_999;

    let resp = client()
        .get(format!("http://{addr}/api/v1/jobs/{unknown_id}"))
        .bearer_auth(&bearer)
        .send()
        .await
        .expect("GET /api/v1/jobs/<inconnu> — serveur doit répondre");

    assert_eq!(
        resp.status(),
        404,
        "GET /api/v1/jobs/<id_inconnu> doit retourner 404 Not Found (RT5 fix)"
    );
}

/// E2E : vault_write sans bearer retourne 401.
///
/// Vérifie que l'auth middleware bloque les requêtes non authentifiées.
#[tokio::test]
async fn vault_write_without_bearer_returns_401() {
    let (addr, _bearer) = spawn_write_server().await;

    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        // Pas de bearer_auth intentionnel
        .json(&serde_json::json!({
            "title": "Test sans auth",
            "body": "Corps",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("vault_write sans bearer — serveur doit répondre");

    assert_eq!(
        resp.status(),
        401,
        "vault_write sans bearer doit retourner 401 Unauthorized"
    );
}
