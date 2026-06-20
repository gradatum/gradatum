//! Régression #32 — FTS5 escape : phrase queries avec ponctuation ne doivent pas déclencher HTTP 500.
//!
//! Symptôme : `query = "2.1.1"` → HTTP 500 `fts5 syntax error near "."`.
//! Cause : le filtre de détection d'opérateurs ne couvrait pas `.`, `,`, `'`, `!`, `?`, etc.
//! Fix : inversion logique — wrap phrase si la query contient TOUT caractère hors `[A-Za-z0-9_ ]`
//! OU si elle contient un mot-clé FTS5 (`AND`, `OR`, `NOT`, `NEAR`).
//!
//! # Structure des tests
//!
//! 6 tests E2E via routeur de test in-memory (pattern `auth_e2e_full_flow.rs`) :
//! 1. `fts_query_with_dot_returns_200`              — query `2.1.1` → 200 pas 500.
//! 2. `fts_query_alpha_dot_returns_200`             — query avec version-like token → 200 pas 500.
//! 3. `fts_query_apostrophe_returns_200`            — query `O'Reilly` → 200 pas 500.
//! 4. `fts_query_dash_and_dot_returns_200`          — query `phase-2.x` → 200 pas 500.
//! 5. `fts_query_simple_alphanumeric_returns_200`   — query `gradatum` → 200 (path tokenizer normal).
//! 6. `fts_query_fts5_keyword_returns_200`          — query `gradatum AND notes` → 200 (wrap phrase).
//!
//! # Auth
//!
//! Chaque test construit un `AppState::with_jwt_and_acl` avec un `JwtService::new_ephemeral()`
//! et un preset ACL `[[consumer]] identity="fts-tester"` autorisant read sur `main/*`.
//! Le token JWT est émis via `jwt.sign(...)` — vérifiable par le même service (clé éphémère isolée).
//!
//! # Résultats attendus
//!
//! Tous les tests vérifient HTTP 200 avec corps JSON conforme `{"items": [...]}`.
//! Le nombre de hits peut être 0 (corpus vide in-memory) — l'important est l'absence du 500
//! et la présence du champ `items` (tableau JSON).
//!
//! Fix #32 — FTS5 escape.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_server::state::AppState;
use http_body_util::BodyExt;
use tower::ServiceExt;

// ── Preset ACL de test ────────────────────────────────────────────────────────

/// Preset ACL autorisant `fts-tester` en lecture sur tous les loci `main/*`.
///
/// L'identité `fts-tester` est utilisée comme `sub` dans le JWT de test.
/// Correspond au format `{tenant_id}/{section}` évalué par `vault_search`.
const TEST_ACL_FTS: &str = r#"
[[consumer]]
identity = "fts-tester"
read_patterns  = ["main/*", "main/main"]
write_patterns = []
"#;

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Construit un routeur de test avec ACL et JWT configurés.
///
/// Retourne `(router, token)` où `token` est un JWT valide signé par le service éphémère.
/// L'ACL autorise `fts-tester` en lecture — le token utilisera cette identité comme `sub`.
async fn build_fts_app() -> (axum::Router, String) {
    use axum::{Router, middleware};

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_FTS)
        .expect("preset ACL FTS valide — invariant statique");

    // Émettre un JWT valide — signé par la clé éphémère, vérifiable par le même service.
    let token = jwt
        .sign(
            "fts-tester",
            &["read".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("signature JWT fts-tester — invariant test");

    let state = AppState::with_jwt_and_acl(jwt, acl);

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state);

    (app, token)
}

/// Construit une requête POST /api/v1/vault_search avec `query` et un bearer JWT.
fn vault_search_req(query: &str, token: &str) -> Request<Body> {
    let body = serde_json::json!({
        "query": query,
        "limit": 5
    });
    Request::builder()
        .uri("/api/v1/vault_search")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Envoie la requête et retourne (status_code, body_json).
async fn send(query: &str) -> (StatusCode, serde_json::Value) {
    let (app, token) = build_fts_app().await;
    let resp = app.oneshot(vault_search_req(query, &token)).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({"raw": "non-json"}));
    (status, json)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Régression #32 — query `2.1.1` (points) ne doit pas déclencher 500.
///
/// Avant le fix : FTS5 recevait `2.1.1` non-quoté → `fts5 syntax error near "."`.
/// Après le fix : wrappé en phrase `"2.1.1"` → FTS5 cherche la phrase littérale → 200.
#[tokio::test]
async fn fts_query_with_dot_returns_200() {
    let (status, json) = send("2.1.1").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "query '2.1.1' avec points doit retourner 200, pas 500. body={json}"
    );
    assert!(
        json["items"].is_array(),
        "réponse doit contenir le champ 'items' (tableau). body={json}"
    );
}

/// Régression #32 — query avec token contenant un point (`alpha` suivi de chiffre) ne doit pas déclencher 500.
#[tokio::test]
async fn fts_query_alpha_dot_returns_200() {
    let (status, json) = send("alpha.8").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "query 'alpha.8' doit retourner 200, pas 500. body={json}"
    );
    assert!(
        json["items"].is_array(),
        "réponse doit contenir le champ 'items'. body={json}"
    );
}

/// Régression #32 — query `O'Reilly` (apostrophe) ne doit pas déclencher 500.
///
/// FTS5 rejette `O'Reilly` non-quoté — l'apostrophe termine le token.
/// Après le fix : wrappé en `"O''Reilly"` (apostrophe doublée dans la phrase FTS5).
#[tokio::test]
async fn fts_query_apostrophe_returns_200() {
    let (status, json) = send("O'Reilly").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "query \"O'Reilly\" doit retourner 200, pas 500. body={json}"
    );
    assert!(
        json["items"].is_array(),
        "réponse doit contenir le champ 'items'. body={json}"
    );
}

/// Régression #32 — query `phase-2.x` (tiret + point) ne doit pas déclencher 500.
///
/// Combine `-` (déjà dans l'ancienne liste) et `.` (absent) — vérifie que les deux
/// sont couverts par la nouvelle détection à inversion logique.
#[tokio::test]
async fn fts_query_dash_and_dot_returns_200() {
    let (status, json) = send("phase-2.x").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "query 'phase-2.x' doit retourner 200, pas 500. body={json}"
    );
    assert!(
        json["items"].is_array(),
        "réponse doit contenir le champ 'items'. body={json}"
    );
}

/// Contrôle positif — query alphanumérique simple `gradatum` → 200.
///
/// Vérifie que le path "query safe" (hors wrap phrase) fonctionne toujours.
/// Résultat items[] peut être vide (corpus in-memory vide) — 200 attendu.
#[tokio::test]
async fn fts_query_simple_alphanumeric_returns_200() {
    let (status, json) = send("gradatum").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "query simple 'gradatum' doit retourner 200. body={json}"
    );
    assert!(
        json["items"].is_array(),
        "réponse doit contenir le champ 'items'. body={json}"
    );
}

/// Contrôle opérateur FTS5 — query `gradatum AND notes` → 200.
///
/// Vérifie que le mot-clé `AND` déclenche toujours le wrap phrase (comportement
/// préservé depuis la détection initiale). La query devient `"gradatum AND notes"` (phrase).
#[tokio::test]
async fn fts_query_fts5_keyword_returns_200() {
    let (status, json) = send("gradatum AND notes").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "query 'gradatum AND notes' doit retourner 200. body={json}"
    );
    assert!(
        json["items"].is_array(),
        "réponse doit contenir le champ 'items'. body={json}"
    );
}
