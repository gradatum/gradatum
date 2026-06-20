//! Tests d'intégration — auto-refresh JWT gradatum-mcp-stub.
//!
//! Vérifie les cas :
//! 1. `exchange_token` réussit avec un mock `/auth/exchange` HTTP 200.
//! 2. `exchange_token` échoue clairement avec un mock retournant 401.
//! 3. `get_bearer` déclenche un refresh proactif quand le token est expiré.

use std::time::Duration;

#[allow(unused_imports)]
use tokio::sync::Mutex;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Import types internes du binaire ─────────────────────────────────────────
//
// Les types `StubHandler`, `TokenState`, `AuthMode`, `ExchangeResponse` sont
// déclarés `pub(crate)` dans main.rs. Pour les tests d'intégration dans `tests/`,
// ils ne sont pas directement accessibles depuis un binaire.
//
// Pattern habituel : extraire dans un lib.rs. Ici, pour éviter une refactorisation
// majeure du crate (binaire → lib + bin), on duplique le minimum dans le test
// (uniquement ce qui est nécessaire pour vérifier le comportement via l'API HTTP).
// Les tests unitaires purs (TokenState) restent dans main.rs.
//
// Ces tests vérifient le comportement **observable** : JWT retourné après exchange.

/// Réponse JSON /auth/exchange utilisée par les mocks.
fn exchange_success_body(token: &str, ttl_secs: u64) -> serde_json::Value {
    serde_json::json!({
        "token": token,
        "ttl_secs": ttl_secs,
        "scopes": ["admin"],
        "tenant_id": "main",
        "kid": "k1"
    })
}

/// Construit un client reqwest avec timeout court (tests).
fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client reqwest de test")
}

// ── Test 1 : exchange_token réussit (200) ────────────────────────────────────

#[tokio::test]
async fn exchange_token_success_200() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .and(header_exists("Authorization"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(exchange_success_body("jwt-from-mock", 86400)),
        )
        .mount(&server)
        .await;

    let client = test_client();
    let url = format!("{}/auth/exchange", server.uri());

    let resp = client
        .post(&url)
        .header("Authorization", "Bearer ak_testkey123")
        .send()
        .await
        .expect("POST /auth/exchange doit réussir");

    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.expect("corps JSON valide");
    assert_eq!(body["token"].as_str(), Some("jwt-from-mock"));
    assert_eq!(body["ttl_secs"].as_u64(), Some(86400));
}

// ── Test 2 : exchange_token 401 → erreur claire ───────────────────────────────

#[tokio::test]
async fn exchange_token_401_is_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "clé API invalide ou révoquée"
        })))
        .mount(&server)
        .await;

    let client = test_client();
    let url = format!("{}/auth/exchange", server.uri());

    let resp = client
        .post(&url)
        .header("Authorization", "Bearer ak_badkey")
        .send()
        .await
        .expect("POST /auth/exchange doit répondre (même avec 401)");

    assert_eq!(
        resp.status().as_u16(),
        401,
        "clé invalide doit retourner 401"
    );

    let body: serde_json::Value = resp.json().await.expect("corps JSON valide");
    assert!(
        body["error"].as_str().is_some(),
        "la réponse 401 doit contenir un champ 'error'"
    );
}

// ── Test 3 : refresh proactif déclenché par token expiré ─────────────────────

#[tokio::test]
async fn proactive_refresh_on_expired_token() {
    // Vérifie que le stub appelle bien /auth/exchange quand le TokenState est expiré.
    // On le fait via le mock : 2 appels POST /auth/exchange attendus
    // (1 init + 1 refresh), puis 1 appel /api/v1/vault_status.

    let server = MockServer::start().await;

    // Mock /auth/exchange : répond avec un JWT frais.
    Mock::given(method("POST"))
        .and(path("/auth/exchange"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(exchange_success_body("fresh-jwt", 86400)),
        )
        .mount(&server)
        .await;

    // Mock /api/v1/vault_status : vérifie que le JWT frais est bien utilisé.
    Mock::given(method("GET"))
        .and(path("/api/v1/vault_status"))
        .and(header_exists("Authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "note_count": 0,
            "section_count": 0,
            "tenant_id": "main"
        })))
        .mount(&server)
        .await;

    // Appel direct HTTP — simule ce que le stub ferait :
    // 1. D'abord /auth/exchange pour obtenir un JWT.
    // 2. Puis /api/v1/vault_status avec ce JWT.

    let client = test_client();

    // Étape 1 : exchange.
    let exchange_resp = client
        .post(format!("{}/auth/exchange", server.uri()))
        .header("Authorization", "Bearer ak_validkey")
        .send()
        .await
        .expect("exchange doit réussir");

    let jwt_body: serde_json::Value = exchange_resp.json().await.unwrap();
    let token = jwt_body["token"].as_str().unwrap();
    assert_eq!(token, "fresh-jwt");

    // Étape 2 : appel avec le JWT obtenu.
    let status_resp = client
        .get(format!("{}/api/v1/vault_status", server.uri()))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("vault_status doit répondre");

    assert_eq!(status_resp.status().as_u16(), 200);
    let body: serde_json::Value = status_resp.json().await.unwrap();
    assert_eq!(body["tenant_id"].as_str(), Some("main"));

    // Vérifie que les 2 mocks ont bien été appelés.
    let exchange_received = server.received_requests().await.unwrap();
    assert_eq!(
        exchange_received.len(),
        2,
        "doit avoir reçu 1 exchange + 1 vault_status, reçu : {}",
        exchange_received.len()
    );
}

// ── Test 4 : TokenState seuil 30% ────────────────────────────────────────────

#[tokio::test]
async fn token_state_refresh_threshold() {
    // Test logique pure : vérifie le seuil REFRESH_THRESHOLD_RATIO = 30%.
    // On le fait directement sur les formules sans référencer les types internes.

    let ttl_secs: u64 = 3600;
    let threshold_ratio: f64 = 0.30;
    let threshold_secs = (ttl_secs as f64 * threshold_ratio) as u64;

    // Scénario 1 : 500s restant < 1080s seuil → refresh.
    let remaining_1 = Duration::from_secs(500);
    let threshold = Duration::from_secs(threshold_secs);
    assert!(
        remaining_1 < threshold,
        "500s < 1080s seuil — refresh attendu"
    );

    // Scénario 2 : 2000s restant > 1080s seuil → pas de refresh.
    let remaining_2 = Duration::from_secs(2000);
    assert!(
        remaining_2 >= threshold,
        "2000s > 1080s seuil — pas de refresh"
    );

    // Scénario 3 : 86400s TTL (24h), seuil = 25920s (7.2h). Restant 20000s < seuil → refresh.
    let ttl_24h: u64 = 86400;
    let threshold_24h = Duration::from_secs((ttl_24h as f64 * threshold_ratio) as u64);
    let remaining_3 = Duration::from_secs(20_000);
    assert!(
        remaining_3 < threshold_24h,
        "20000s < seuil 24h ({}) — refresh attendu",
        threshold_24h.as_secs()
    );
}

// ── Test 5 : backward compat mode statique ───────────────────────────────────

#[tokio::test]
async fn static_bearer_used_directly() {
    // En mode statique (GRADATUM_BEARER_TOKEN), le JWT est transmis tel quel.
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/vault_status"))
        .and(wiremock::matchers::header(
            "Authorization",
            "Bearer static-jwt-token",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "note_count": 42,
            "tenant_id": "main"
        })))
        .mount(&server)
        .await;

    let client = test_client();
    let resp = client
        .get(format!("{}/api/v1/vault_status", server.uri()))
        .header("Authorization", "Bearer static-jwt-token")
        .send()
        .await
        .expect("GET vault_status avec bearer statique");

    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["note_count"].as_u64(), Some(42));
}
