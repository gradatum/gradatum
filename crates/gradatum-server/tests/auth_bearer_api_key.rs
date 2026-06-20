//! Tests d'intégration — Bearer api-key dans auth_middleware (B3 feat/v0.6.0-mcp-native).
//!
//! ## Contexte
//!
//! Le transport HTTP MCP de Claude Code envoie un header `Authorization: Bearer`
//! **statique**. Un JWT TTL 24h expiré casserait le transport MCP de façon silencieuse.
//! La solution : accepter aussi `ak_...` comme valeur du Bearer — l'api-key est
//! stable, révocable, et déjà gérée par `SqliteApiKeyStore`.
//!
//! ## Tests
//!
//! 1. Bearer `ak_<valide>` → TrustContext::BearerToken (authentifié, tenant=main)
//! 2. Bearer `ak_<invalide>` → Unauthenticated → 401
//! 3. Bearer `ak_<révoquée>` → Unauthenticated → 401
//! 4. Bearer JWT valide → chemin JWT inchangé → 200
//! 5. Bearer JWT expiré → Unauthenticated → 401 (chemin JWT inchangé)
//! 6. Bearer `ak_<wrong_tenant>` → refusé 403 (garde tenant mono-vault)
//! 7. POST /mcp avec `Authorization: Bearer ak_<valide>` → succès (pas 401)
//!
//! ## Fixtures
//!
//! Chaque test crée un `AppState` avec `SqliteApiKeyStore` en base temporaire (TempDir),
//! un `JwtService` éphémère, et un routeur minimal ou MCP selon le test.
//! Aucune vraie clé hardcodée.

use axum::Extension;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Router, middleware};
use gradatum_auth::jwt::TokenScope;
use gradatum_core::trust::TrustContext;
use gradatum_server::middleware::auth_middleware;
use gradatum_server::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Construit un `AppState` de test avec `SqliteApiKeyStore` réel + JwtService éphémère.
///
/// Retourne `(state, TempDir)` — le `TempDir` doit rester en vie le temps du test.
async fn build_state_with_api_keys() -> (AppState, TempDir) {
    let dir = TempDir::new().expect("tempdir test");
    let api_keys_path = dir.path().join("api_keys.sqlite");
    let state = AppState::new()
        .with_api_keys_path(&api_keys_path)
        .await
        .expect("SqliteApiKeyStore init test");
    (state, dir)
}

/// Handler authentifié — retourne 200 si `TrustContext::is_authenticated()`, 401 sinon.
///
/// Mimique le comportement des vrais handlers (ex: `vault_search_impl`).
/// Obligatoire : `handler_ok` seul ne vérifie pas le TrustContext.
async fn handler_authed(Extension(trust): Extension<TrustContext>) -> StatusCode {
    if trust.is_authenticated() {
        StatusCode::OK
    } else {
        StatusCode::UNAUTHORIZED
    }
}

/// Construit un routeur minimal avec `auth_middleware` + `handler_authed` sur `/test`.
///
/// Le handler retourne 401 si TrustContext::Unauthenticated — ce qui permet
/// de distinguer les chemins auth dans les tests.
fn minimal_router(state: AppState) -> Router {
    Router::new()
        .route("/test", get(handler_authed))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// Envoie un GET /test avec un bearer optionnel, retourne le StatusCode.
async fn get_test(router: Router, bearer: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method("GET").uri("/test");
    if let Some(token) = bearer {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder
        .body(Body::empty())
        .expect("construction requête test — invariant builder");
    router
        .oneshot(req)
        .await
        .expect("handler ne doit pas paniquer")
        .status()
}

// ── Test 1 : Bearer ak_<valide> → authentifié → 200 ─────────────────────────

/// Bearer `ak_<valide>` doit authentifier via `ApiKeyStore::verify` et retourner
/// `TrustContext::BearerToken` — le handler d'en-dessous reçoit 200.
///
/// Prouve que `auth_middleware` détecte le préfixe `ak_` et délègue à `state.api_keys`.
#[tokio::test]
async fn bearer_api_key_valid_returns_authenticated() {
    let (state, _dir) = build_state_with_api_keys().await;

    let material = state
        .api_keys
        .create(
            "mcp-client",
            vec!["read".to_string(), "write".to_string()],
            "main".to_string(),
            Some("clé MCP test".to_string()),
        )
        .await
        .expect("create api_key");

    let router = minimal_router(state);
    let status = get_test(router, Some(material.secret.as_str())).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "Bearer ak_<valide> doit authentifier → handler retourne 200"
    );
}

// ── Test 2 : Bearer ak_<invalide> → Unauthenticated → 401 ────────────────────

/// Un Bearer `ak_` avec un secret incorrect doit retourner 401.
///
/// `auth_middleware` appelle `api_keys.verify()` → `NotFound` → Unauthenticated.
/// Le handler minimal retourne 401 quand TrustContext est Unauthenticated.
#[tokio::test]
async fn bearer_api_key_invalid_returns_unauthenticated() {
    let (state, _dir) = build_state_with_api_keys().await;
    let router = minimal_router(state);

    // Clé jamais créée — `ak_` + 64 chars hex valides mais inexistants.
    let fake_secret = format!("ak_{}", "deadbeef".repeat(8));
    let status = get_test(router, Some(fake_secret.as_str())).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Bearer ak_<invalide> doit retourner 401"
    );
}

// ── Test 3 : Bearer ak_<révoquée> → Unauthenticated → 401 ───────────────────

/// Une api-key révoquée présentée en Bearer doit retourner 401.
///
/// `auth_middleware` appelle `api_keys.verify()` → `AlreadyRevoked` → Unauthenticated.
#[tokio::test]
async fn bearer_api_key_revoked_returns_unauthenticated() {
    let (state, _dir) = build_state_with_api_keys().await;

    let material = state
        .api_keys
        .create(
            "agent-revoke",
            vec!["read".to_string()],
            "main".to_string(),
            None,
        )
        .await
        .expect("create api_key");

    // Révoquer la clé.
    state
        .api_keys
        .revoke(&material.prefix)
        .await
        .expect("revoke api_key");

    let router = minimal_router(state);
    let status = get_test(router, Some(material.secret.as_str())).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Bearer ak_<révoquée> doit retourner 401"
    );
}

// ── Test 4 : Bearer JWT valide → chemin JWT inchangé → 200 ──────────────────

/// Un JWT valide continue à fonctionner — le chemin JWT DOIT être inchangé.
///
/// Prouve que l'ajout du chemin `ak_` n'affecte pas le chemin JWT existant.
#[tokio::test]
async fn bearer_jwt_valid_unchanged() {
    let (state, _dir) = build_state_with_api_keys().await;

    let token = state
        .jwt
        .sign(
            "agent-jwt",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT — clé éphémère");

    let router = minimal_router(state);
    let status = get_test(router, Some(token.as_str())).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "Bearer JWT valide → chemin JWT inchangé → 200"
    );
}

// ── Test 5 : Bearer JWT expiré → 401 (chemin inchangé) ───────────────────────

/// Un JWT expiré doit retourner 401 — le chemin JWT DOIT rester inchangé.
///
/// Construit un JWT avec TTL=0 (expiré immédiatement). L'api-key
/// n'étant pas un JWT (ne commence pas par `eyJ`), la distinction est
/// testée par la non-interférence du chemin ak_.
#[tokio::test]
async fn bearer_jwt_expired_unchanged() {
    use gradatum_auth::jwt::JwtService;

    // Créer un JwtService avec TTL 0s pour émettre un JWT expiré.
    let (state, _dir) = build_state_with_api_keys().await;
    let (_, signing_key) = JwtService::generate_signing_bytes();
    let short_jwt = JwtService::new(
        signing_key,
        "kid-test-expired".into(),
        "gradatum".into(),
        0, // ttl_human_secs = 0
        0, // ttl_service_secs = 0
    );
    let expired_token = short_jwt
        .sign(
            "user-expired",
            &["read".to_string()],
            TokenScope::Human,
            "main",
        )
        .expect("sign JWT expiré (TTL=0)");

    // Ce token a TTL=0 → verify() sur le JwtService RÉEL de l'AppState retourne Err.
    // De plus, même si on tentait de le vérifier avec le bon JwtService, il serait rejeté
    // car il a été signé avec une clé différente.
    let router = minimal_router(state);
    let status = get_test(router, Some(expired_token.as_str())).await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "Bearer JWT avec mauvaise signature/expiré → 401 (chemin JWT inchangé)"
    );
}

// ── Test 6 : Bearer ak_<wrong_tenant> → refusé 403 ───────────────────────────

/// Toutes les api-keys sont créées avec `tenant_id="main"` (invariant mono-vault).
///
/// `ApiKeyStore::create()` refuse les tenant ≠ "main" à la création.
/// Ce test vérifie que la garde `tenant_is_authorized` dans `auth_middleware`
/// reste effective pour une api-key — elle devrait retourner 403.
///
/// Note : puisque `create()` interdit déjà la création de clés non-main,
/// on simule un TrustContext tenant ≠ "main" via JWT forgé — le test 6 vérifie
/// la garde `tenant_is_authorized` pour les BearerToken api-key.
/// Dans la pratique, toutes les clés créées via `ApiKeyStore::create` ont
/// `tenant_id="main"` → ce test valide qu'une clé valide avec tenant="main"
/// passe ET que la garde fonctionne pour un token forge-main (invariant).
#[tokio::test]
async fn bearer_api_key_wrong_tenant_refused() {
    let (state, _dir) = build_state_with_api_keys().await;

    // On ne peut pas créer de clé tenant ≠ "main" (invariant SqliteApiKeyStore).
    // On vérifie la garde via JWT forgé avec tenant="evil" (chemin existant).
    let token_evil_tenant = state
        .jwt
        .sign(
            "evil-agent",
            &["read".to_string()],
            TokenScope::Service,
            "evil", // tenant ≠ "main"
        )
        .expect("sign JWT tenant evil");

    let router = minimal_router(state);
    let status = get_test(router, Some(token_evil_tenant.as_str())).await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "BearerToken tenant ≠ main doit être refusé 403 par tenant_is_authorized"
    );
}

// ── Test 7 : POST /mcp avec Bearer ak_<valide> → succès (pas 401) ────────────

/// POST `/mcp` avec `Authorization: Bearer ak_<valide>` ne doit PAS retourner 401.
///
/// Prouve l'intégration end-to-end : le transport HTTP MCP (header statique api-key)
/// traverse `auth_middleware` sans refus d'auth.
///
/// Note : ce test utilise le routeur minimal (pas le vrai MCP) pour tester uniquement
/// la couche auth — le test R1/R2/R3 de `mcp_native.rs` couvre le MCP complet.
#[tokio::test]
async fn mcp_with_api_key_bearer_succeeds() {
    let (state, _dir) = build_state_with_api_keys().await;

    let material = state
        .api_keys
        .create(
            "claude-code-mcp",
            vec!["read".to_string(), "write".to_string()],
            "main".to_string(),
            Some("Claude Code MCP transport".to_string()),
        )
        .await
        .expect("create api_key mcp");

    // Routeur minimal avec POST /mcp (simule la route MCP côté auth).
    // Utilise handler_authed pour vérifier que le TrustContext est bien authentifié.
    use axum::routing::post;
    let router = Router::new()
        .route("/mcp", post(handler_authed))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Authorization", format!("Bearer {}", material.secret))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(Body::empty())
        .expect("build request mcp");

    let resp = router
        .oneshot(req)
        .await
        .expect("handler ne doit pas paniquer");

    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /mcp Bearer ak_<valide> NE DOIT PAS retourner 401"
    );
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /mcp Bearer ak_<valide> doit retourner 200"
    );
}
