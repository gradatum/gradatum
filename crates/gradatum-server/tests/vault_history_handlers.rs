//! Tests d'intégration — 4 handlers MCP history F-40 (v0.4.0).
//!
//! Vérifie pour chaque handler :
//! - **401 UNAUTHORIZED** si pas de header `Authorization`.
//! - **403 FORBIDDEN** si bearer présent mais ACL refuse (default deny).
//! - **200 OK** (résultat vide/erreur gracieuse) avec bearer valid et ACL Allow.
//!
//! Les tests 200 complets (avec vault réel) sont couverts par les tests d'intégration
//! de `gradatum-vault` (tests/cow_history.rs). Ici on vérifie le routing HTTP
//! + auth gate + shape de la réponse JSON.
//!
//! # Helpers
//!
//! Deux serveurs de test :
//! - `start_test_server_deny()` : ACL default deny (pour tester 401/403).
//! - `start_test_server_allow()` : ACL avec preset Read+Write sur `main/main`
//!   (pour tester 200 avec PlaceholderRegistry).

use std::net::SocketAddr;
use std::time::Duration;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_server::state::AppState;
use reqwest::StatusCode;
use serde_json::json;

/// Preset ACL : autorise `test-user` en lecture + écriture sur `main/main`.
const TEST_ACL_HISTORY: &str = r#"
[[consumer]]
identity = "test-user"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

// ── Middleware trust stub ─────────────────────────────────────────────────────

/// Inject TrustContext depuis le header Authorization.
async fn trust_stub(
    mut req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use gradatum_core::trust::TrustContext;
    let trust = if let Some(auth) = req.headers().get(axum::http::header::AUTHORIZATION) {
        if let Ok(val) = auth.to_str() {
            if let Some(token) = val.strip_prefix("Bearer ") {
                if !token.is_empty() {
                    TrustContext::BearerToken {
                        kid: "test-kid".to_string(),
                        aud: "gradatum".to_string(),
                        sub: token.into(),
                        scopes: vec!["read".to_string(), "write".to_string()],
                        tenant_id: "main".into(),
                        jti: None,
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

// ── Serveurs de test ──────────────────────────────────────────────────────────

/// Serveur avec ACL default deny (tester auth gates 401/403).
async fn start_test_server_deny() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_server::api_v1;

    let state = AppState::default();
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

/// Serveur avec ACL Allow sur main/main (tester 200 avec PlaceholderRegistry).
///
/// Utilise `AppState::with_jwt_and_acl` avec un preset autorisant `test-user`
/// en lecture + écriture sur `main/*`. Le `trust_stub` met `sub = "test-user"`.
async fn start_test_server_allow() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_server::api_v1;

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_HISTORY)
        .expect("preset ACL history valide — invariant statique");
    let state = AppState::with_jwt_and_acl(jwt, acl);

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

/// Client reqwest sans retry, timeout 5s.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("construction client HTTP — pas de TLS custom")
}

// ── Tests 401 (sans bearer) ───────────────────────────────────────────────────

/// vault_history — POST sans bearer → 401.
#[tokio::test]
async fn vault_history_401_unauthenticated() {
    let addr = start_test_server_deny().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_history", addr))
        .json(&json!({ "note_id": "01JT00000000000000000000A1" }))
        .send()
        .await
        .expect("requête vault_history sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_history sans bearer doit retourner 401"
    );
}

/// vault_history_get — POST sans bearer → 401.
#[tokio::test]
async fn vault_history_get_401_unauthenticated() {
    let addr = start_test_server_deny().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_history_get", addr))
        .json(&json!({ "note_id": "01JT00000000000000000000A1", "ts_ms": 1700000000000_i64 }))
        .send()
        .await
        .expect("requête vault_history_get sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_history_get sans bearer doit retourner 401"
    );
}

/// vault_restore — POST sans bearer → 401.
#[tokio::test]
async fn vault_restore_401_unauthenticated() {
    let addr = start_test_server_deny().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_restore", addr))
        .json(&json!({ "note_id": "01JT00000000000000000000A1", "ts_ms": 1700000000000_i64 }))
        .send()
        .await
        .expect("requête vault_restore sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_restore sans bearer doit retourner 401"
    );
}

/// vault_diff — POST sans bearer → 401.
#[tokio::test]
async fn vault_diff_401_unauthenticated() {
    let addr = start_test_server_deny().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_diff", addr))
        .json(&json!({ "note_id": "01JT00000000000000000000A1", "a": "current", "b": "current" }))
        .send()
        .await
        .expect("requête vault_diff sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_diff sans bearer doit retourner 401"
    );
}

// ── Tests 403 (bearer présent, ACL deny) ─────────────────────────────────────

/// vault_history — POST avec bearer mais ACL default deny → 403.
#[tokio::test]
async fn vault_history_403_acl_deny() {
    let addr = start_test_server_deny().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_history", addr))
        .header("Authorization", "Bearer test-user")
        .json(&json!({ "note_id": "01JT00000000000000000000A1" }))
        .send()
        .await
        .expect("requête vault_history avec bearer ACL deny");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_history avec ACL deny doit retourner 403"
    );
}

/// vault_restore — POST avec bearer mais ACL default deny → 403 (vérifie ACL Write).
#[tokio::test]
async fn vault_restore_403_acl_deny() {
    let addr = start_test_server_deny().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_restore", addr))
        .header("Authorization", "Bearer test-user")
        .json(&json!({ "note_id": "01JT00000000000000000000A1", "ts_ms": 1700000000000_i64 }))
        .send()
        .await
        .expect("requête vault_restore avec bearer ACL deny");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_restore avec ACL deny doit retourner 403"
    );
}

// ── Tests 200 avec PlaceholderRegistry ───────────────────────────────────────

/// vault_history — 200 OK avec versions vide (PlaceholderRegistry).
///
/// PlaceholderRegistry retourne `Ok(vec![])` → réponse 200 `{ versions: [], count: 0 }`.
#[tokio::test]
async fn vault_history_200_placeholder_empty() {
    let addr = start_test_server_allow().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_history", addr))
        .header("Authorization", "Bearer test-user")
        .json(&json!({ "note_id": "01JT00000000000000000000A1" }))
        .send()
        .await
        .expect("requête vault_history avec bearer ACL allow");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vault_history avec ACL allow doit retourner 200"
    );
    let body: serde_json::Value = resp
        .json()
        .await
        .expect("réponse vault_history doit être du JSON");
    assert_eq!(
        body["versions"],
        json!([]),
        "PlaceholderRegistry doit retourner une liste vide"
    );
    assert_eq!(body["count"], 0, "count doit être 0 si aucun snapshot");
}

/// vault_diff — 400 Bad Request si sélecteur invalide (ni timestamp ni 'current').
///
/// La validation des sélecteurs est faite AVANT l'appel vault → 400.
#[tokio::test]
async fn vault_diff_400_invalid_selector() {
    let addr = start_test_server_allow().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_diff", addr))
        .header("Authorization", "Bearer test-user")
        .json(&json!({
            "note_id": "01JT00000000000000000000A1",
            "a": "invalid-selector",
            "b": "current"
        }))
        .send()
        .await
        .expect("requête vault_diff sélecteur invalide");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "vault_diff avec sélecteur invalide doit retourner 400"
    );
}

/// vault_diff — 200 OK avec diff vide (deux versions 'current' sur PlaceholderRegistry).
///
/// PlaceholderRegistry : `history_diff` retourne Err(Storage) → le handler doit
/// mapper en 500. Ce test vérifie la propagation de l'erreur.
#[tokio::test]
async fn vault_diff_500_placeholder_storage_error() {
    let addr = start_test_server_allow().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_diff", addr))
        .header("Authorization", "Bearer test-user")
        .json(&json!({
            "note_id": "01JT00000000000000000000A1",
            "a": "current",
            "b": "current"
        }))
        .send()
        .await
        .expect("requête vault_diff current/current PlaceholderRegistry");
    // PlaceholderRegistry::history_diff retourne Err(Storage("placeholder vault..."))
    // → map_err_to_status → 500 (message ne contient pas "introuvable").
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "vault_diff sur PlaceholderRegistry (pas de vault réel) doit retourner 500"
    );
}

/// vault_history_get — 404 sur note inexistante (PlaceholderRegistry retourne NoteNotFound).
#[tokio::test]
async fn vault_history_get_404_note_not_found() {
    let addr = start_test_server_allow().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_history_get", addr))
        .header("Authorization", "Bearer test-user")
        .json(&json!({
            "note_id": "01JT00000000000000000000A1",
            "ts_ms": 1700000000000_i64
        }))
        .send()
        .await
        .expect("requête vault_history_get NoteNotFound");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "vault_history_get sur note inexistante doit retourner 404"
    );
}

/// vault_restore — 500 sur PlaceholderRegistry (pas de vault réel).
#[tokio::test]
async fn vault_restore_500_placeholder_storage_error() {
    let addr = start_test_server_allow().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_restore", addr))
        .header("Authorization", "Bearer test-user")
        .json(&json!({
            "note_id": "01JT00000000000000000000A1",
            "ts_ms": 1700000000000_i64
        }))
        .send()
        .await
        .expect("requête vault_restore PlaceholderRegistry");
    // PlaceholderRegistry::history_restore retourne Err(Storage("placeholder vault..."))
    // → map_err_to_status → 500.
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "vault_restore sur PlaceholderRegistry doit retourner 500"
    );
}
