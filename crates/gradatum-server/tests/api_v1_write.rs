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
    use axum::{Router, middleware, routing::get};
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

    use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
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
    // Le détail job exige désormais une auth (fix authz F-16 : GET jobs = AclOp::Read
    // sur main/jobs) — le poll_url est consommé par un client authentifié.
    let poll_resp = client()
        .get(format!("http://{}{}", addr, poll_url))
        .bearer_auth(TEST_CONSUMER_SUB)
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

// ── Test 5 : vault_write expose note_id préalloué dans la réponse (P2 item 1) ──

/// `POST /api/v1/vault_write` → 202 doit contenir un champ `note_id` (ULID valide).
///
/// Vérifie que le ULID préalloué est bien exposé dans la réponse 202, sans poll.
/// Critère P2 item 1 : `vault_read` avec ce `note_id` ne doit pas nécessiter un poll.
#[tokio::test]
async fn vault_write_response_contains_note_id_ulid() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_write", addr))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "Note avec note_id préalloué",
            "body": "Vérifie que note_id est exposé dans la réponse 202.",
            "section_hint": "decisions"
        }))
        .send()
        .await
        .expect("requête vault_write");

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "vault_write doit retourner 202"
    );

    let body: serde_json::Value = resp
        .json()
        .await
        .expect("réponse vault_write doit être du JSON valide");

    // note_id doit être présent et être un ULID valide (26 chars alphanum).
    let note_id = body["note_id"]
        .as_str()
        .expect("réponse 202 doit contenir note_id (champ string)");

    assert_eq!(
        note_id.len(),
        26,
        "note_id doit être un ULID 26 chars — reçu: {note_id}"
    );
    assert!(
        note_id.chars().all(|c| c.is_ascii_alphanumeric()),
        "note_id doit être alphanumérique — reçu: {note_id}"
    );

    // note_id doit être parsable en ULID.
    ulid::Ulid::from_string(note_id)
        .unwrap_or_else(|e| panic!("note_id doit être un ULID valide — {note_id}: {e}"));

    // note_id et job_id doivent être distincts (ce sont deux ULIDs différents).
    let job_id = body["job_id"]
        .as_str()
        .expect("réponse 202 doit contenir job_id");
    assert_ne!(
        note_id, job_id,
        "note_id et job_id doivent être des ULIDs distincts"
    );
}

/// Non-régression : les champs existants (job_id, status, poll_url) sont préservés.
///
/// Vérifie que l'ajout de `note_id` est purement additif — aucun champ existant
/// n'est modifié ou supprimé (rétrocompat P2 item 1).
#[tokio::test]
async fn vault_write_note_id_is_additive_existing_fields_preserved() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_write", addr))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "Rétrocompat champs existants",
            "body": "job_id + status + poll_url inchangés."
        }))
        .send()
        .await
        .expect("requête vault_write");

    let body: serde_json::Value = resp.json().await.expect("réponse JSON valide");

    // Champs existants préservés.
    assert!(body.get("job_id").is_some(), "job_id préservé");
    assert_eq!(body["status"], "queued", "status = 'queued' préservé");
    assert!(
        body["poll_url"]
            .as_str()
            .map(|s| s.starts_with("/api/v1/jobs/"))
            .unwrap_or(false),
        "poll_url préservé"
    );
    // Champ additionnel.
    assert!(body.get("note_id").is_some(), "note_id ajouté (P2 item 1)");
}

// ── Fix B : résolution req.note_id (Task 2) ──────────────────────────────────
//
// Le harness start_write_test_server() n'injecte PAS de vault réel
// (PlaceholderRegistry) → read_note_by_id renvoie toujours NoteNotFound →
// la garde overwrite 409 n'est jamais déclenchée ici (cas a/b/c).

#[tokio::test]
async fn vault_write_absent_note_id_generates_fresh_ulid() {
    let addr = start_write_test_server().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "t", "body": "b", "section_hint": "decisions", "tenant_id": "main"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value = resp.json().await.unwrap();
    let nid = body["note_id"].as_str().unwrap();
    assert!(
        ulid::Ulid::from_string(nid).is_ok(),
        "note_id doit être un ULID valide"
    );
}

#[tokio::test]
async fn vault_write_invalid_note_id_returns_400() {
    let addr = start_write_test_server().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "t", "body": "b", "section_hint": "decisions",
            "tenant_id": "main", "note_id": "not-a-ulid"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn vault_write_valid_note_id_absent_in_vault_is_honored() {
    let addr = start_write_test_server().await;
    let provided = "01KTYB000000000000000000AA"; // ULID valide, introuvable (PlaceholderRegistry)
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "t", "body": "b", "section_hint": "decisions",
            "tenant_id": "main", "note_id": provided
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["note_id"].as_str().unwrap(), provided);
}

// ── Fix B Task 4 : cas g — ACL deny 403 ──────────────────────────────────────

#[tokio::test]
async fn vault_write_acl_deny_returns_403() {
    let addr = start_write_test_server().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth("unknown-consumer-no-acl")
        .json(&serde_json::json!({
            "title": "t", "body": "b", "section_hint": "decisions", "tenant_id": "main"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ── V3 (F-02) : DefaultBodyLimit 256 KiB sur vault_write ─────────────────────

/// Un body > 256 KiB envoyé sur `POST /api/v1/vault_write` → 413 Content Too Large.
///
/// Protège contre DoS : body volumineux × nombreux wikilinks → RAM worker.
/// Cap 256 KiB justifié pour une note texte (cohérent avec les autres endpoints).
#[tokio::test]
async fn vault_write_body_over_256kib_returns_413() {
    let addr = start_write_test_server().await;
    // Body volumineux : 257 KiB de contenu brut.
    let oversized_body = "x".repeat(257 * 1024);
    let payload = format!(
        r#"{{"title":"titre","body":"{oversized_body}"}}"#,
        oversized_body = oversized_body
    );
    let resp = client()
        .post(format!("http://{}/api/v1/vault_write", addr))
        .bearer_auth(TEST_CONSUMER_SUB)
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .expect("requête vault_write oversized");

    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "body > 256 KiB doit retourner 413 Payload Too Large"
    );
}

// ── Task 5 : validateur de schéma project-map à l'écriture ────────────────────

/// Une écriture `section_hint=project-map` avec le triple-obligatoire
/// (project + status + kind) est acceptée (202).
#[tokio::test]
async fn vault_write_project_map_valid_schema_returns_202() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "Carte feature X",
            "body": "Suivi de la feature.\n\n[[project:gradatum]] [[status:IN_PROGRESS]] [[kind:FEATURE]] [[version:gradatum/0.6.1]]",
            "section_hint": "project-map",
            "tenant_id": "main"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "carte project-map valide doit être acceptée (202)"
    );
}

/// Une écriture `section_hint=project-map` SANS lien `project:` est rejetée (400).
#[tokio::test]
async fn vault_write_project_map_missing_project_returns_400() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "Carte invalide",
            "body": "Pas de project ni status.\n\n[[kind:FIX]]",
            "section_hint": "project-map",
            "tenant_id": "main"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "carte project-map hors-schéma doit être rejetée (400)"
    );
}

/// Une écriture `section_hint=decisions` n'est PAS soumise au validateur
/// project-map (aucune contrainte de liens) → 202 même sans aucun wikilink.
#[tokio::test]
async fn vault_write_decisions_section_is_not_validated() {
    let addr = start_write_test_server().await;
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(TEST_CONSUMER_SUB)
        .json(&serde_json::json!({
            "title": "Décision libre",
            "body": "Corps sans aucun wikilink typé project-map.",
            "section_hint": "decisions",
            "tenant_id": "main"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "section decisions ne doit subir aucune validation project-map"
    );
}
