//! Tests d'intégration AUTH-T5 — endpoint `POST /auth/exchange`.
//!
//! Vérifie le flux complet :
//! - Clé API valide → 200 + token JWT vérifiable
//! - Header absent → 400
//! - Secret invalide → 401
//! - Clé révoquée → 401
//! - Route montée AVANT le middleware JWT (pas de JWT requis pour s'échanger)
//! - V7 scope opt-in : sans `scope` → TTL 24 h (Service), avec `scope=human` → TTL 1 h (Human)

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_core::scope::AgentId;
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
    use axum::{Router, middleware, routing::get};
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
        .create(
            &AgentId::new("mcp-stub"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
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
        .create(
            &AgentId::new("agent-1"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
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
        .create(
            &AgentId::new("owner-x"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
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
        .create(
            &AgentId::new("owner-y"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
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

/// SÉCU P0 cross-tenant — Lot 1 : une clé API au tenant ≠ "main" ne doit JAMAIS
/// pouvoir échanger contre un JWT tant que le vault est mono-physique "main".
///
/// La clé est insérée puis mutée en SQL direct (`tenant_id = 'evil'`) pour simuler
/// une clé legacy non-main (la garde Lot 6 empêche désormais leur création via
/// `ApiKeyStore::create`). `/auth/exchange` est l'UNIQUE émetteur de JWT : le gater
/// ici garantit qu'aucun bearer non-main ne peut exister en aval.
#[tokio::test]
async fn exchange_non_main_tenant_key_returns_403() {
    let dir = TempDir::new().expect("tempdir");
    let api_keys_path = dir.path().join("api_keys.sqlite");
    let state = AppState::new()
        .with_api_keys_path(&api_keys_path)
        .await
        .expect("api_keys store init");

    // Créer une clé "main" valide (chemin nominal autorisé).
    let material = state
        .api_keys
        .create(
            &AgentId::new("legacy-evil"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create api key");

    // Muter le tenant en SQL direct pour simuler une clé non-main legacy.
    let conn = rusqlite::Connection::open(&api_keys_path).expect("open api_keys sqlite");
    conn.execute(
        "UPDATE api_keys SET tenant_id = ?1 WHERE prefix = ?2",
        rusqlite::params!["evil", material.prefix],
    )
    .expect("mutate tenant to evil");
    drop(conn);

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
        StatusCode::FORBIDDEN,
        "clé API tenant ≠ main → 403 (aucun mint JWT, invariant mono-vault)"
    );
}

/// Vérifier que /auth/exchange est accessible sans JWT (route non soumise au middleware).
#[tokio::test]
async fn exchange_does_not_require_jwt_in_auth_header() {
    let (state, _dir) = build_test_state().await;

    let material = state
        .api_keys
        .create(
            &AgentId::new("owner-z"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
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

// ── V7 — TTL scope opt-in ────────────────────────────────────────────────────

/// V7-a : échange sans champ `scope` → TokenScope::Service → exp ≈ 24 h.
///
/// Garantit que engine/admin (qui envoient Body::empty()) ne sont pas cassés.
/// Vérifie via le claim `exp` du JWT signé.
#[tokio::test]
async fn exchange_without_scope_field_uses_service_ttl_24h() {
    let (state, _dir) = build_test_state().await;

    let material = state
        .api_keys
        .create(
            &AgentId::new("engine"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create api key");

    let jwt_service = state.jwt.clone();
    let router = build_test_router(state);

    // Aucun body JSON — comportement actuel engine/admin préservé.
    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {}", material.secret))
        .body(Body::empty())
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    assert_eq!(resp.status(), StatusCode::OK, "sans scope → 200");

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 64)
        .await
        .expect("body read");
    let parsed: ExchangeResponse = serde_json::from_slice(&body).expect("parse JSON");

    let claims = jwt_service.verify(&parsed.token).expect("token vérifiable");

    // exp ≈ iat + 86400 (±5 s de marge pour le temps d'exécution du test).
    let delta = claims.exp.saturating_sub(claims.iat);
    assert!(
        (86395..=86405).contains(&delta),
        "TTL Service attendu ≈ 86400 s, obtenu delta = {delta}"
    );
    assert_eq!(parsed.ttl_secs, jwt_service.ttl_service_secs());
}

/// V7-b : échange avec `scope = "human"` → TokenScope::Human → exp ≈ 1 h.
///
/// Vérifie que le studio (LoginPage.tsx) obtiendra bien un token court.
/// Champ optionnel : tout le reste du système ignorant ce champ reste inchangé.
#[tokio::test]
async fn exchange_with_scope_human_uses_human_ttl_1h() {
    let (state, _dir) = build_test_state().await;

    let material = state
        .api_keys
        .create(
            &AgentId::new("studio-user"),
            vec!["vault_read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create api key");

    let jwt_service = state.jwt.clone();
    let router = build_test_router(state);

    // Body JSON avec scope = "human".
    let body_json = r#"{"scope":"human"}"#;
    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {}", material.secret))
        .header("Content-Type", "application/json")
        .body(Body::from(body_json))
        .expect("build request");

    let resp = router.oneshot(req).await.expect("service call");
    assert_eq!(resp.status(), StatusCode::OK, "scope=human → 200");

    let resp_body = axum::body::to_bytes(resp.into_body(), 1024 * 64)
        .await
        .expect("body read");
    let parsed: ExchangeResponse = serde_json::from_slice(&resp_body).expect("parse JSON");

    let claims = jwt_service.verify(&parsed.token).expect("token vérifiable");

    // exp ≈ iat + 3600 (±5 s de marge).
    let delta = claims.exp.saturating_sub(claims.iat);
    assert!(
        (3595..=3605).contains(&delta),
        "TTL Human attendu ≈ 3600 s, obtenu delta = {delta}"
    );
    assert_eq!(parsed.ttl_secs, jwt_service.ttl_human_secs());
}

// ── C3a (F-45) — levée du verrou tenant sous `multi_tenant.enabled = true` ─────

/// Fixture ON : state avec index SQLite réel (tables `tenants`/`tenant_vault_grants`,
/// seed `main`↔`main`), store api_keys réel, flag `multi_tenant` activé.
async fn build_multi_tenant_state() -> (AppState, std::path::PathBuf, TempDir) {
    use gradatum_server::config::{MultiTenantConfig, ServerConfig};

    let dir = TempDir::new().expect("tempdir");
    let api_keys_path = dir.path().join("api_keys.sqlite");
    let index_path = dir.path().join("index.db");
    let state = AppState::new()
        .with_api_keys_path(&api_keys_path)
        .await
        .expect("api_keys store init")
        .with_search_path(&index_path)
        .await
        .expect("SqliteIndex::open — migrations")
        .with_server_config(ServerConfig {
            multi_tenant: MultiTenantConfig { enabled: true },
            ..ServerConfig::default()
        });
    (state, index_path, dir)
}

/// Crée une clé API puis mute son tenant en SQL direct (la création non-main est
/// gardée par un opt-in opérateur — hors sujet ici, on teste l'ÉMISSION du JWT).
async fn create_key_with_tenant(state: &AppState, dir: &TempDir, tenant: &str) -> String {
    let material = state
        .api_keys
        .create(
            &AgentId::new("c3a-owner"),
            vec!["read".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create api key");
    let api_keys_path = dir.path().join("api_keys.sqlite");
    let conn = rusqlite::Connection::open(&api_keys_path).expect("open api_keys sqlite");
    conn.execute(
        "UPDATE api_keys SET tenant_id = ?1 WHERE prefix = ?2",
        rusqlite::params![tenant, material.prefix],
    )
    .expect("mutate tenant");
    drop(conn);
    material.secret
}

/// Provisionne le tenant `research` (statut paramétrable) + self-grant write.
fn seed_tenant(index_path: &std::path::Path, tenant: &str, status: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO tenants (id, status, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![tenant, status, now],
    )
    .expect("seed tenant");
    conn.execute(
        "INSERT INTO tenant_vault_grants (tenant_id, vault_id, access) VALUES (?1, ?1, 'write')",
        rusqlite::params![tenant],
    )
    .expect("seed self-grant");
}

async fn post_exchange(router: axum::Router, secret: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {secret}"))
        .body(Body::empty())
        .expect("build request");
    let resp = router.oneshot(req).await.expect("service call");
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .expect("body read")
        .to_vec();
    (status, body)
}

/// ON : une clé d'un tenant provisionné (actif + grant) obtient un JWT portant
/// SON tenant — l'émission est gouvernée par la même allow-list que le middleware.
#[tokio::test]
async fn exchange_on_provisioned_tenant_mints_scoped_jwt() {
    let (state, index_path, dir) = build_multi_tenant_state().await;
    seed_tenant(&index_path, "research", "active");
    let secret = create_key_with_tenant(&state, &dir, "research").await;
    let jwt_service = state.jwt.clone();

    let (status, body) = post_exchange(build_test_router(state), &secret).await;
    assert_eq!(status, StatusCode::OK, "tenant provisionné → 200");
    let parsed: ExchangeResponse = serde_json::from_slice(&body).expect("parse JSON");
    assert_eq!(parsed.tenant_id, "research");
    let claims = jwt_service.verify(&parsed.token).expect("JWT vérifiable");
    assert_eq!(
        claims.tenant_id, "research",
        "le JWT porte le tenant de la clé"
    );
}

/// ON : tenant jamais provisionné → 403 fail-closed (aucun mint).
#[tokio::test]
async fn exchange_on_unprovisioned_tenant_returns_403() {
    let (state, _index_path, dir) = build_multi_tenant_state().await;
    let secret = create_key_with_tenant(&state, &dir, "ghost").await;

    let (status, _body) = post_exchange(build_test_router(state), &secret).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "tenant non provisionné → 403"
    );
}

/// ON : tenant soft-deleted → 403 (le JOIN `tenants.status = 'active'` de
/// l'allow-list gouverne aussi l'émission).
#[tokio::test]
async fn exchange_on_deleted_tenant_returns_403() {
    let (state, index_path, dir) = build_multi_tenant_state().await;
    seed_tenant(&index_path, "research", "deleted");
    let secret = create_key_with_tenant(&state, &dir, "research").await;

    let (status, _body) = post_exchange(build_test_router(state), &secret).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "tenant soft-deleted → 403");
}

/// ON : les clés `main` continuent d'échanger normalement (seed migration 0030).
#[tokio::test]
async fn exchange_on_main_key_still_works() {
    let (state, _index_path, _dir) = build_multi_tenant_state().await;
    let material = state
        .api_keys
        .create(
            &AgentId::new("mcp-stub"),
            vec!["service".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create api key");

    let (status, _body) = post_exchange(build_test_router(state), &material.secret).await;
    assert_eq!(status, StatusCode::OK, "clé main à ON → 200 (seed 0030)");
}
