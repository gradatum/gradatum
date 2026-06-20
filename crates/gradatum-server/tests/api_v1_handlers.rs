//! Tests d'intégration — 10 handlers MCP read (T8).
//!
//! Vérifie pour chaque handler :
//! - **401 UNAUTHORIZED** si pas de header `Authorization` (pas de TrustContext authentifié).
//!
//! Les tests 200/403 sont couverts dans la suite T12 (parity tests) qui câble un vrai preset ACL.
//! T8 vérifie uniquement le routing + auth gate.
//!
//! # Démarrage du serveur de test
//!
//! Un serveur Axum est démarré sur un port aléatoire (bind `127.0.0.1:0`) pour chaque
//! test. Le serveur utilise `AppState::default()` avec ACL vide (default deny) et
//! middleware TrustContext stub.

use std::net::SocketAddr;
use std::time::Duration;

use gradatum_server::state::AppState;
use reqwest::StatusCode;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Démarre un serveur Axum de test sur un port éphémère et retourne son adresse.
///
/// Le serveur tourne dans une tâche tokio détachée — il sera arrêté à la fin
/// du processus de test.
async fn start_test_server() -> SocketAddr {
    use axum::{Router, middleware, routing::get};
    use gradatum_server::api_v1;

    // Middleware trust stub identique à main.rs (extraction bearer → BearerToken ou Unauthenticated).
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
                            scopes: vec!["read".to_string()],
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
    // Laisser le serveur démarrer.
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

// ── Tests 401 unauthenticated ────────────────────────────────────────────────

/// vault_search — POST sans bearer → 401.
#[tokio::test]
async fn vault_search_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_search", addr))
        .json(&serde_json::json!({ "query": "test" }))
        .send()
        .await
        .expect("requête vault_search sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_search sans bearer doit retourner 401"
    );
}

/// vault_read — POST sans bearer → 401.
#[tokio::test]
async fn vault_read_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_read", addr))
        .json(&serde_json::json!({ "path": "decisions/test" }))
        .send()
        .await
        .expect("requête vault_read sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_read sans bearer doit retourner 401"
    );
}

/// vault_list — POST sans bearer → 401.
#[tokio::test]
async fn vault_list_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_list", addr))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("requête vault_list sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_list sans bearer doit retourner 401"
    );
}

/// vault_status — GET sans bearer → 401.
#[tokio::test]
async fn vault_status_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .get(format!("http://{}/api/v1/vault_status", addr))
        .send()
        .await
        .expect("requête vault_status sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_status sans bearer doit retourner 401"
    );
}

/// vault_graph — POST sans bearer → 401.
#[tokio::test]
async fn vault_graph_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_graph", addr))
        .json(&serde_json::json!({ "root": "decisions/test" }))
        .send()
        .await
        .expect("requête vault_graph sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_graph sans bearer doit retourner 401"
    );
}

/// vault_links — POST sans bearer → 401.
#[tokio::test]
async fn vault_links_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_links", addr))
        .json(&serde_json::json!({ "path": "decisions/test" }))
        .send()
        .await
        .expect("requête vault_links sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_links sans bearer doit retourner 401"
    );
}

/// vault_trace — POST sans bearer → 401.
#[tokio::test]
async fn vault_trace_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_trace", addr))
        .json(&serde_json::json!({ "query": "architecture" }))
        .send()
        .await
        .expect("requête vault_trace sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_trace sans bearer doit retourner 401"
    );
}

/// vault_context — POST sans bearer → 401.
#[tokio::test]
async fn vault_context_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_context", addr))
        .json(&serde_json::json!({ "query": "architecture rust" }))
        .send()
        .await
        .expect("requête vault_context sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_context sans bearer doit retourner 401"
    );
}

/// vault_authors — GET sans bearer → 401.
#[tokio::test]
async fn vault_authors_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .get(format!("http://{}/api/v1/vault_authors", addr))
        .send()
        .await
        .expect("requête vault_authors sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_authors sans bearer doit retourner 401"
    );
}

/// vault_tags — GET sans bearer → 401.
#[tokio::test]
async fn vault_tags_401_unauthenticated() {
    let addr = start_test_server().await;
    let resp = client()
        .get(format!("http://{}/api/v1/vault_tags", addr))
        .send()
        .await
        .expect("requête vault_tags sans bearer");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_tags sans bearer doit retourner 401"
    );
}

// ── Tests 403 FORBIDDEN (bearer présent mais ACL default deny) ───────────────
// Ces tests vérifient que le bearer stub est bien extrait, mais que l'ACL vide
// retourne FORBIDDEN pour tout consumer inconnu (default deny).

/// vault_search — bearer présent, ACL default deny → 403.
#[tokio::test]
async fn vault_search_403_acl_default_deny() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_search", addr))
        .bearer_auth("test-token-stub")
        .json(&serde_json::json!({ "query": "test" }))
        .send()
        .await
        .expect("requête vault_search avec bearer stub");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_search avec bearer non autorisé doit retourner 403 (ACL default deny)"
    );
}

/// vault_list — bearer présent, ACL default deny → 403.
#[tokio::test]
async fn vault_list_403_acl_default_deny() {
    let addr = start_test_server().await;
    let resp = client()
        .post(format!("http://{}/api/v1/vault_list", addr))
        .bearer_auth("test-token-stub")
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("requête vault_list avec bearer stub");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_list avec bearer non autorisé doit retourner 403 (ACL default deny)"
    );
}
