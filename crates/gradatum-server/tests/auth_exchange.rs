//! Tests d'intégration AUTH-T5 — endpoint `POST /auth/exchange`.
//!
//! Vérifie le flux complet :
//! - Clé API valide → 200 + token JWT vérifiable
//! - Header absent → 400
//! - Secret invalide → 401
//! - Clé révoquée → 401
//! - Route montée AVANT le middleware JWT (pas de JWT requis pour s'échanger)

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_server::auth_routes::ExchangeResponse;
use gradatum_server::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;

/// Construit un `AppState` de test avec `SqliteApiKeyStore` réel + JwtService éphémère.
async fn build_test_state() -> (AppState, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let api_keys_path = dir.path().join("api_keys.sqlite");
    let state = AppState::new()
        .with_api_keys_path(&api_keys_path)
        .await
        .expect("api_keys store init");
    (state, dir)
}

/// Construit le routeur de test avec la route /auth/exchange.
fn build_test_router(state: AppState) -> axum::Router {
    use axum::{middleware, routing::get, Router};
    use gradatum_server::health;

    // Même logique que build_router dans main.rs — routes auth hors middleware.
    let authed = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ));

    let unauthed = Router::new()
        .route("/health", get(health::handler))
        .merge(gradatum_server::auth_routes::router());

    authed.merge(unauthed).with_state(state)
}

/// Flux nominal : clé valide → 200 + token JWT.
#[tokio::test]
async fn exchange_valid_key_returns_jwt() {
    let (state, _dir) = build_test_state().await;

    // Créer une clé API dans le store.
    let material = state
        .api_keys
        .create("mcp-stub", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create api key");

    let jwt_service = state.jwt.clone();
    let router = build_test_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {}", material.secret))
        .body(Body::empty())
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    assert_eq!(resp.status(), StatusCode::OK, "échange valide → 200");

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 64)
        .await
        .expect("body read");
    let parsed: ExchangeResponse = serde_json::from_slice(&body).expect("parse JSON");

    // Le token doit être vérifiable avec le JwtService.
    let claims = jwt_service
        .verify(&parsed.token)
        .expect("token émis par /auth/exchange doit être vérifiable");

    assert_eq!(claims.sub, "mcp-stub");
    assert_eq!(claims.tenant_id, "main");
    assert!(claims.scopes.contains(&"vault_read".to_string()));
    assert!(parsed.ttl_secs > 0);
}

/// Format alternatif sans "Bearer " prefix.
#[tokio::test]
async fn exchange_accepts_bare_ak_prefix() {
    let (state, _dir) = build_test_state().await;

    let material = state
        .api_keys
        .create("agent-1", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create api key");

    let router = build_test_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", &material.secret) // sans "Bearer "
        .body(Body::empty())
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "format ak_ sans Bearer → 200"
    );
}

/// Header Authorization absent → 400.
#[tokio::test]
async fn exchange_missing_header_returns_400() {
    let (state, _dir) = build_test_state().await;
    let router = build_test_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .body(Body::empty())
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "header absent → 400"
    );
}

/// Header Authorization présent mais format non-ak_ → 400.
#[tokio::test]
async fn exchange_wrong_format_returns_400() {
    let (state, _dir) = build_test_state().await;
    let router = build_test_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header(
            "Authorization",
            format!("Bearer {}", "not-an-api-key-format"),
        )
        .body(Body::empty())
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "format non-ak_ → 400"
    );
}

/// Secret incorrect → 401.
#[tokio::test]
async fn exchange_wrong_secret_returns_401() {
    let (state, _dir) = build_test_state().await;

    // Créer une clé mais passer un mauvais secret.
    state
        .api_keys
        .create("owner-x", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create");

    let router = build_test_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer ak_{}", "0".repeat(32)))
        .body(Body::empty())
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "mauvais secret → 401"
    );
}

/// Clé révoquée → 401.
#[tokio::test]
async fn exchange_revoked_key_returns_401() {
    let (state, _dir) = build_test_state().await;

    let material = state
        .api_keys
        .create("owner-y", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create");

    state
        .api_keys
        .revoke(&material.prefix)
        .await
        .expect("revoke");

    let router = build_test_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {}", material.secret))
        .body(Body::empty())
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "clé révoquée → 401"
    );
}

/// Vérifier que /auth/exchange est accessible sans JWT (route non soumise au middleware).
#[tokio::test]
async fn exchange_does_not_require_jwt_in_auth_header() {
    let (state, _dir) = build_test_state().await;

    let material = state
        .api_keys
        .create("owner-z", vec!["vault_read".into()], "main".into(), None)
        .await
        .expect("create");

    let router = build_test_router(state);

    // La requête n'a que l'en-tête Authorization avec l'API key (pas de JWT).
    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {}", material.secret))
        .body(Body::empty())
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    // Doit réussir — prouve que le middleware JWT n'intercepte pas cette route.
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/auth/exchange accessible sans JWT préexistant"
    );
}
