//! Tests E2E `POST /api/v1/vault_classify` — heuristique offline (F-37 classify).
//!
//! Couvre les 4 cas exigés par la tâche :
//!
//! 1. `vault_classify_heuristic_returns_200` — note existante → 200 + champs conformes
//!    (`method="heuristic"`, `confidence ∈ {0.0, 0.5, 0.9}`, `note_id`, sections présentes).
//! 2. `vault_classify_unauthenticated_returns_401` — sans bearer → 401.
//! 3. `vault_classify_unknown_note_returns_404` — note absente → 404.
//! 4. `vault_classify_invalid_ulid_returns_400` — ULID invalide → 400.
//!
//! # Auth
//!
//! Depuis F-1, un bearer JWT valide est exigé. Les tests 401 et 400 n'ont pas besoin
//! d'un vault réel (l'erreur est retournée avant la lecture du vault).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::TokenScope;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

/// Preset ACL autorisant read+write sur `main/*`.
const ACL_ALLOW: &str = r#"
[[consumer]]
identity = "classify-e2e-tester"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

// ── Helper : app avec vault réel + token ─────────────────────────────────────

/// Construit une app de test avec un `Vault` réel (TempDir) et un token JWT signé.
///
/// Retourne `(app, vault, token, _dir)`.
/// Le `TempDir` est retourné pour maintenir le répertoire vivant pendant le test.
async fn build_app_with_vault() -> (axum::Router, Arc<Vault>, String, TempDir) {
    use axum::{Router, middleware};
    use gradatum_server::state::AppState;

    let dir = TempDir::new().expect("TempDir vault_classify_e2e — répertoire temporaire");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test vault_classify_e2e"),
    );

    let mut state = AppState::new().with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>);
    state.acl = Arc::new(AclEngine::from_preset_str(ACL_ALLOW).expect("preset ACL valide"));

    let token = state
        .jwt
        .sign(
            "classify-e2e-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT de test — clé éphémère AppState::new()");

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, vault, token, dir)
}

/// App minimale sans vault réel — pour tester les cas d'erreur précoces (401, 400, 404).
async fn build_app_without_vault() -> (axum::Router, String) {
    use axum::{Router, middleware};
    use gradatum_server::state::AppState;

    let mut state = AppState::new();
    state.acl = Arc::new(AclEngine::from_preset_str(ACL_ALLOW).expect("preset ACL valide"));

    let token = state
        .jwt
        .sign(
            "classify-e2e-tester",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT de test");

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, token)
}

// ── Helper : écriture d'une note dans le vault de test ────────────────────────

/// Crée une note minimale dans le vault concret et retourne son ULID sous forme de `String`.
///
/// Section : `decisions` — choisie pour maximiser la probabilité d'un outcome `Admitted`
/// ou `Pending` par l'heuristique (section nommée explicitement dans le titre).
async fn seed_vault_note(vault: &Vault) -> String {
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: chrono::Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let note = vault
        .write_note(fm, "# Decision de test\n\nCorps de la note.".into())
        .await
        .expect("write_note — invariant seed_vault_note");
    note.id.to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Test 1 : classify note existante → 200 + réponse JSON conforme.
///
/// Vérifie :
/// - Status HTTP 200.
/// - `note_id` reflète celui envoyé.
/// - `method` == `"heuristic"` (zéro LLM).
/// - `confidence` ∈ `{0.0, 0.5, 0.9}` (trois valeurs discrètes).
/// - `current_section` et `suggested_section` sont des chaînes non vides.
#[tokio::test]
async fn vault_classify_heuristic_returns_200() {
    let (app, vault, token, _dir) = build_app_with_vault().await;
    let note_id = seed_vault_note(&vault).await;

    let req = Request::builder()
        .uri("/api/v1/vault_classify")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "note_id": note_id })).unwrap(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "vault_classify doit retourner 200"
    );

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(json["note_id"], note_id, "note_id doit être reflété");
    assert_eq!(
        json["method"], "heuristic",
        "method doit être 'heuristic' (zéro LLM)"
    );

    let confidence = json["confidence"]
        .as_f64()
        .expect("confidence doit être un nombre");
    assert!(
        confidence == 0.9 || confidence == 0.5 || confidence == 0.0,
        "confidence doit être l'une des trois valeurs discrètes (0.9/0.5/0.0), obtenu {confidence}"
    );

    assert!(
        json["current_section"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "current_section doit être une chaîne non vide"
    );
    assert!(
        json["suggested_section"]
            .as_str()
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "suggested_section doit être une chaîne non vide"
    );
}

/// Test 2 : sans bearer → 401 UNAUTHORIZED.
///
/// Vérifie que l'auth est vérifiée avant toute I/O vault.
#[tokio::test]
async fn vault_classify_unauthenticated_returns_401() {
    // App sans vault réel — l'erreur 401 survient avant la lecture du vault.
    let (app, _token) = build_app_without_vault().await;
    let fake_id = Ulid::generate().to_string();

    let req = Request::builder()
        .uri("/api/v1/vault_classify")
        .method("POST")
        .header("content-type", "application/json")
        // Pas de header Authorization.
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "note_id": fake_id })).unwrap(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_classify sans bearer doit retourner 401"
    );
}

/// Test 3 : note absente du vault → 404 NOT FOUND.
///
/// Vérifie que la lecture d'une note inexistante produit 404, pas 500.
#[tokio::test]
async fn vault_classify_unknown_note_returns_404() {
    // App sans vault réel — DefaultVault retourne NoteNotFound sur tout read.
    let (app, token) = build_app_without_vault().await;
    let absent_id = Ulid::generate().to_string();

    let req = Request::builder()
        .uri("/api/v1/vault_classify")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "note_id": absent_id })).unwrap(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "vault_classify avec note absente doit retourner 404"
    );
}

/// Test 4 : ULID invalide → 400 BAD REQUEST.
///
/// Vérifie la validation d'entrée (parse-don't-validate à la frontière).
#[tokio::test]
async fn vault_classify_invalid_ulid_returns_400() {
    // App sans vault réel — la validation ULID survient avant la lecture du vault.
    let (app, token) = build_app_without_vault().await;

    let req = Request::builder()
        .uri("/api/v1/vault_classify")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "note_id": "pas-un-ulid-valide" })).unwrap(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "vault_classify avec ULID invalide doit retourner 400"
    );
}
