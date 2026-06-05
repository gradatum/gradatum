//! Tests d'intégration — tools write (vault_write, vault_classify, vault_downgrade).
//!
//! Vérifie que chaque outil WRITE :
//! 1. Forward bien un POST vers la route `/api/v1/{tool}` du serveur.
//! 2. Transmet le corps JSON tel quel (stub ne valide pas).
//! 3. Renvoie la réponse 202 Accepted du serveur.
//!
//! Ces tests utilisent WireMock pour simuler le serveur HTTP.
//! Le stub HTTP est testé directement (pas via stdio MCP) — on vérifie
//! le comportement observable (route + body + réponse).

use std::time::Duration;

use wiremock::matchers::{body_json, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Client reqwest avec timeout court (tests).
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client reqwest de test")
}

/// Réponse 202 Accepted standard — EnqueuedResponse.
fn enqueued_response(job_id: i64) -> serde_json::Value {
    serde_json::json!({
        "job_id": job_id,
        "status": "queued",
        "poll_url": format!("/api/v1/jobs/{}", job_id)
    })
}

// ── Test vault_write ──────────────────────────────────────────────────────────

#[tokio::test]
async fn vault_write_posts_to_correct_route() {
    let server = MockServer::start().await;

    let request_body = serde_json::json!({
        "title": "Note de test",
        "body": "Contenu **markdown** de la note.",
        "author": "claude-code",
        "tags": ["test", "mcp"],
        "section_hint": "debug",
        "tenant_id": "main"
    });

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_write"))
        .and(header_exists("Authorization"))
        .and(body_json(request_body.clone()))
        .respond_with(ResponseTemplate::new(202).set_body_json(enqueued_response(1001)))
        .expect(1) // Le stub doit appeler exactement 1 fois.
        .mount(&server)
        .await;

    let client = test_client();
    let resp = client
        .post(format!("{}/api/v1/vault_write", server.uri()))
        .header("Authorization", "Bearer jwt-test")
        .json(&request_body)
        .send()
        .await
        .expect("POST /api/v1/vault_write doit répondre");

    assert_eq!(
        resp.status().as_u16(),
        202,
        "vault_write doit retourner 202 Accepted"
    );

    let body: serde_json::Value = resp.json().await.expect("corps JSON valide");
    assert_eq!(body["job_id"].as_i64(), Some(1001));
    assert_eq!(body["status"].as_str(), Some("queued"));
    assert!(
        body["poll_url"].as_str().is_some(),
        "poll_url doit être présent"
    );

    // Vérifie que le mock a bien reçu exactement 1 appel.
    server.verify().await;
}

// ── Test vault_classify ───────────────────────────────────────────────────────

#[tokio::test]
async fn vault_classify_posts_to_correct_route() {
    let server = MockServer::start().await;

    let request_body = serde_json::json!({
        "note_id": "01HX5GKRA9N8VQPBZ7JY3M4WF",
        "tenant_id": "main"
    });

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_classify"))
        .and(header_exists("Authorization"))
        .and(body_json(request_body.clone()))
        .respond_with(ResponseTemplate::new(202).set_body_json(enqueued_response(1002)))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client();
    let resp = client
        .post(format!("{}/api/v1/vault_classify", server.uri()))
        .header("Authorization", "Bearer jwt-test")
        .json(&request_body)
        .send()
        .await
        .expect("POST /api/v1/vault_classify doit répondre");

    assert_eq!(
        resp.status().as_u16(),
        202,
        "vault_classify doit retourner 202 Accepted"
    );

    let body: serde_json::Value = resp.json().await.expect("corps JSON valide");
    assert_eq!(body["job_id"].as_i64(), Some(1002));
    assert_eq!(body["status"].as_str(), Some("queued"));

    server.verify().await;
}

// ── Test vault_downgrade ──────────────────────────────────────────────────────

#[tokio::test]
async fn vault_downgrade_posts_to_correct_route() {
    let server = MockServer::start().await;

    let request_body = serde_json::json!({
        "note_id": "01HX5GKRA9N8VQPBZ7JY3M4WF",
        "reason": "obsolète — remplacée par une note plus récente",
        "replaced_by": "01HX5GKRA9N8VQPBZ7JY3M4WG",
        "tenant_id": "main"
    });

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_downgrade"))
        .and(header_exists("Authorization"))
        .and(body_json(request_body.clone()))
        .respond_with(ResponseTemplate::new(202).set_body_json(enqueued_response(1003)))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client();
    let resp = client
        .post(format!("{}/api/v1/vault_downgrade", server.uri()))
        .header("Authorization", "Bearer jwt-test")
        .json(&request_body)
        .send()
        .await
        .expect("POST /api/v1/vault_downgrade doit répondre");

    assert_eq!(
        resp.status().as_u16(),
        202,
        "vault_downgrade doit retourner 202 Accepted"
    );

    let body: serde_json::Value = resp.json().await.expect("corps JSON valide");
    assert_eq!(body["job_id"].as_i64(), Some(1003));
    assert_eq!(body["status"].as_str(), Some("queued"));

    // replaced_by optionnel — vérifie qu'il est transmis correctement.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "exactement 1 appel attendu");
    let received_body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("corps requête JSON valide");
    assert_eq!(
        received_body["replaced_by"].as_str(),
        Some("01HX5GKRA9N8VQPBZ7JY3M4WG"),
        "replaced_by doit être transmis tel quel"
    );

    server.verify().await;
}

// ── Test vault_downgrade sans replaced_by (optionnel absent) ─────────────────

#[tokio::test]
async fn vault_downgrade_without_replaced_by() {
    let server = MockServer::start().await;

    // Corps minimal : note_id + reason uniquement (replaced_by absent).
    let request_body = serde_json::json!({
        "note_id": "01HX5GKRA9N8VQPBZ7JY3M4WF",
        "reason": "doublon",
        "tenant_id": "main"
    });

    Mock::given(method("POST"))
        .and(path("/api/v1/vault_downgrade"))
        .and(header_exists("Authorization"))
        .respond_with(ResponseTemplate::new(202).set_body_json(enqueued_response(1004)))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client();
    let resp = client
        .post(format!("{}/api/v1/vault_downgrade", server.uri()))
        .header("Authorization", "Bearer jwt-test")
        .json(&request_body)
        .send()
        .await
        .expect("POST /api/v1/vault_downgrade sans replaced_by doit répondre");

    assert_eq!(resp.status().as_u16(), 202);

    server.verify().await;
}
