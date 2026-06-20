//! Tests de sécurité F-1 (A01 Broken Access Control) — auth+ACL sur notes.rs.
//!
//! # Contexte (F-1)
//!
//! Avant ce fix, les 3 handlers de `notes.rs` (`vault_downgrade`, `patch_note`,
//! `move_note_locus`) reçoivent `Extension<TrustContext>` via le middleware, mais
//! ne le consomment jamais : aucun `is_authenticated()`, aucun check ACL.
//! Ces opérations étant des **mutations directes** sur le SQLite index (sync, pas de
//! queue), elles constituaient un trou authz explicite documenté dans le module :
//! « Auth — These endpoints do not require a bearer JWT (private network assumed). »
//!
//! Le fix ajoute la séquence obligatoire is_authenticated → effective_tenant →
//! acl.evaluate en tête de chaque `*_impl` dans `logic.rs`, avant toute I/O.
//!
//! # Matrice testée (par endpoint)
//!
//! | Endpoint                   | sans bearer → 401 | bearer valide + ACL OK → nominal | bearer valide + ACL deny → 403 |
//! |----------------------------|-------------------|----------------------------------|--------------------------------|
//! | POST `/vault_downgrade`    | ✓                 | ✓                                | ✓                              |
//! | PATCH `/notes/<id>`        | ✓                 | ✓                                | ✓                              |
//! | POST `/notes/<id>/move`    | ✓                 | ✓                                | ✓                              |
//!
//! # Setup
//!
//! Même pattern que `jobs_auth.rs` : SqliteIndex in-memory pour les tests
//! vault_downgrade/patch_note, Vault TempDir pour move_note_locus (mutation physique .md).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Router, middleware};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::TokenScope;
use gradatum_core::index::Index;
use gradatum_index::SqliteIndex;
use gradatum_server::{api_v1, middleware::auth_middleware, state::AppState};
use std::sync::Arc;
use tower::ServiceExt;
use ulid::Ulid;

// ─── Presets ACL ─────────────────────────────────────────────────────────────

/// Identité du consumer de test.
const TEST_IDENTITY: &str = "f1-security-tester";

/// ACL autorisant read+write sur `main/*`.
const ACL_ALLOW: &str = r#"
[[consumer]]
identity = "f1-security-tester"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

/// ACL où le consumer existe mais N'A AUCUN droit → authentifié mais 403.
const ACL_DENY: &str = r#"
[[consumer]]
identity = "f1-security-tester"
read_patterns  = []
write_patterns = []
"#;

// ─── Helpers de construction ──────────────────────────────────────────────────

/// Construit un `(AppState, token JWT valide)` avec index SQLite in-memory + preset ACL.
async fn build_state_with_index(acl_preset: &str) -> (AppState, String, Arc<SqliteIndex>) {
    let idx = Arc::new(
        SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex::open_in_memory() — invariant test security_f1"),
    );

    let mut state = AppState::new();
    state.search = Arc::clone(&idx) as Arc<dyn Index>;

    let acl = AclEngine::from_preset_str(acl_preset).expect("preset ACL valide");
    state.acl = Arc::new(acl);

    let token = state
        .jwt
        .sign(
            TEST_IDENTITY,
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT de test");

    (state, token, idx)
}

/// Construit le Router de test avec `auth_middleware` monté.
fn build_router(state: AppState) -> Router {
    Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// Insère une note minimale dans l'index concret et retourne son ULID.
async fn seed_note(idx: &SqliteIndex) -> String {
    let id = Ulid::new().to_string();
    idx.seed_note(&id, "reference", "test note pour security_f1")
        .await
        .expect("seed_note — doit réussir sur index in-memory");
    id
}

// ─────────────────────────────────────────────────────────────────────────────
// §1 — POST /api/v1/vault_downgrade
// ─────────────────────────────────────────────────────────────────────────────

/// POST /vault_downgrade SANS bearer → 401.
///
/// Preuve que la faille F-1 est fermée : avant le fix, ce POST renvoyait 200/404
/// sans aucune vérification d'auth. Après le fix → 401 avant toute I/O.
#[tokio::test]
async fn vault_downgrade_returns_401_when_unauthenticated() {
    let (state, _token, idx) = build_state_with_index(ACL_ALLOW).await;
    let note_id = seed_note(&idx).await;
    let router = build_router(state);

    let body = serde_json::json!({"note_id": note_id, "reason": "test-401"});
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_downgrade")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build request sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /vault_downgrade sans bearer DOIT être 401 (faille F-1 fermée)"
    );
}

/// POST /vault_downgrade avec bearer valide + ACL Write OK → 200 (comportement nominal préservé).
#[tokio::test]
async fn vault_downgrade_returns_200_when_authenticated_and_acl_allows() {
    let (state, token, idx) = build_state_with_index(ACL_ALLOW).await;
    let note_id = seed_note(&idx).await;
    let router = build_router(state);

    let body =
        serde_json::json!({"note_id": note_id, "reason": "test-nominal", "tenant_id": "main"});
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_downgrade")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build request authed");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /vault_downgrade authentifié + ACL Write → 200"
    );
}

/// POST /vault_downgrade avec bearer valide + ACL Write refusée → 403.
#[tokio::test]
async fn vault_downgrade_returns_403_when_acl_denies() {
    let (state, token, idx) = build_state_with_index(ACL_DENY).await;
    let note_id = seed_note(&idx).await;
    let router = build_router(state);

    let body = serde_json::json!({"note_id": note_id, "reason": "test-403", "tenant_id": "main"});
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_downgrade")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build request acl-deny");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "POST /vault_downgrade ACL Write refusée → 403"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// §2 — PATCH /api/v1/notes/<id>
// ─────────────────────────────────────────────────────────────────────────────

/// PATCH /notes/<id> SANS bearer → 401.
#[tokio::test]
async fn patch_note_returns_401_when_unauthenticated() {
    let (state, _token, idx) = build_state_with_index(ACL_ALLOW).await;
    let note_id = seed_note(&idx).await;
    let router = build_router(state);

    let body = serde_json::json!({"status_reason": "test-401"});
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/api/v1/notes/{note_id}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build PATCH sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "PATCH /notes/<id> sans bearer DOIT être 401 (faille F-1 fermée)"
    );
}

/// PATCH /notes/<id> avec bearer valide + ACL Write OK → 204 (comportement nominal préservé).
///
/// On patche `status_reason` (chemin SQL direct) — ce chemin ne nécessite pas le vault
/// réel (patch via search.patch_note_status), donc fonctionne avec PlaceholderRegistry.
#[tokio::test]
async fn patch_note_returns_204_when_authenticated_and_acl_allows() {
    let (state, token, idx) = build_state_with_index(ACL_ALLOW).await;
    let note_id = seed_note(&idx).await;
    let router = build_router(state);

    let body = serde_json::json!({"status_reason": "test-nominal"});
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/api/v1/notes/{note_id}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build PATCH authed");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "PATCH /notes/<id> authentifié + ACL Write → 204"
    );
}

/// PATCH /notes/<id> avec bearer valide + ACL Write refusée → 403.
#[tokio::test]
async fn patch_note_returns_403_when_acl_denies() {
    let (state, token, idx) = build_state_with_index(ACL_DENY).await;
    let note_id = seed_note(&idx).await;
    let router = build_router(state);

    let body = serde_json::json!({"status_reason": "test-403"});
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/api/v1/notes/{note_id}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build PATCH acl-deny");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "PATCH /notes/<id> ACL Write refusée → 403"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// §3 — POST /api/v1/notes/<id>/move
// ─────────────────────────────────────────────────────────────────────────────

/// POST /notes/<id>/move SANS bearer → 401.
#[tokio::test]
async fn move_note_locus_returns_401_when_unauthenticated() {
    let (state, _token, _idx) = build_state_with_index(ACL_ALLOW).await;
    let note_id = Ulid::new().to_string();
    let router = build_router(state);

    let body = serde_json::json!({"locus": "archive"});
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/notes/{note_id}/move"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build move sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /notes/<id>/move sans bearer DOIT être 401 (faille F-1 fermée)"
    );
}

/// POST /notes/<id>/move avec bearer valide + ACL Write OK → 404 (note absente,
/// vault PlaceholderRegistry ne connaît pas la note, mais auth passe).
///
/// Prouve que l'auth est vérifiée ET que le chemin métier est atteint (404 ≠ 401/403).
#[tokio::test]
async fn move_note_locus_passes_auth_reaches_business_logic() {
    let (state, token, _idx) = build_state_with_index(ACL_ALLOW).await;
    let note_id = Ulid::new().to_string();
    let router = build_router(state);

    let body = serde_json::json!({"locus": "archive"});
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/notes/{note_id}/move"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build move authed");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "POST /notes/<id>/move authentifié + ACL Write → logique métier atteinte (vault=404)"
    );
}

/// POST /notes/<id>/move avec bearer valide + ACL Write refusée → 403.
#[tokio::test]
async fn move_note_locus_returns_403_when_acl_denies() {
    let (state, token, _idx) = build_state_with_index(ACL_DENY).await;
    let note_id = Ulid::new().to_string();
    let router = build_router(state);

    let body = serde_json::json!({"locus": "archive"});
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/notes/{note_id}/move"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .expect("build move acl-deny");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "POST /notes/<id>/move ACL Write refusée → 403"
    );
}
