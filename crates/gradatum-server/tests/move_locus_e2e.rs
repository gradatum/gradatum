//! Tests E2E `POST /api/v1/notes/{id}/move` — move to locus (F-37 S1.4 + D1.1).
//!
//! Depuis D1.1 (v0.4.8), le move est une **relocalisation physique** du `.md` via le
//! chemin Vault (`vault.move_locus`), cohérente de bout en bout (`.md` + index + CoW).
//!
//! Depuis F-1 (fix A01 Broken Access Control), un bearer JWT valide est exigé.
//! Les tests nominaux (`move_success_persists_locus`, `move_unknown_note_is_404`)
//! passent désormais un token. Les tests de validation HTTP précoce (400/422) ne
//! nécessitent pas de token (la validation ULID/locus/JSON précède le check auth).
//!
//! Couvre :
//! 0. `move_success_persists_locus` — 204 + locus persisté (vérifié via `vault_read`).
//! 1. `move_bad_ulid_is_400` — id non-ULID rejeté (avant auth — 400 sans token).
//! 2. `move_invalid_locus_is_400` — locus invalide (avant auth — 400 sans token).
//! 3. `move_unknown_note_is_404` — note absente (après auth — 404 avec token).
//! 4. `move_unknown_field_is_422` — body avec champ inconnu (avant auth — 422 sans token).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::TokenScope;
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

/// Preset ACL autorisant read+write sur `main/*`.
const ACL_ALLOW: &str = r#"
[[consumer]]
identity = "move-locus-tester"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

/// Construit une app de test avec un Vault réel (TempDir) ET un token JWT.
///
/// Retourne `(app, vault, token, _dir)`. Le token est signé avec l'ACL_ALLOW.
/// Le `TempDir` est retourné pour maintenir le répertoire vivant pendant le test.
async fn build_app_with_vault_and_token() -> (axum::Router, Arc<Vault>, String, TempDir) {
    use axum::{Router, middleware};
    use gradatum_core::scope::VaultId;
    use gradatum_server::state::AppState;

    let dir = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );

    let mut state = AppState::new().with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>);
    state.acl = Arc::new(AclEngine::from_preset_str(ACL_ALLOW).expect("preset ACL valide"));

    let token = state
        .jwt
        .sign(
            "move-locus-tester",
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

    (app, vault, token, dir)
}

/// App minimale sans Vault réel + token JWT — pour les tests qui atteignent l'auth
/// mais pas une note réelle (ex: move_unknown_note_is_404).
async fn build_app_with_token() -> (axum::Router, String) {
    use axum::{Router, middleware};
    use gradatum_server::state::AppState;

    let mut state = AppState::new();
    state.acl = Arc::new(AclEngine::from_preset_str(ACL_ALLOW).expect("preset ACL valide"));

    let token = state
        .jwt
        .sign(
            "move-locus-tester",
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

/// App minimale sans Vault réel (PlaceholderRegistry) — suffisante pour les cas
/// d'erreur qui ne touchent pas une note réelle ET qui se produisent avant l'auth
/// (400/422 sur ULID invalide, locus invalide, champ inconnu).
async fn build_app() -> axum::Router {
    use axum::{Router, middleware};
    use gradatum_server::state::AppState;

    let state = AppState::new();
    Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone())
}

/// Construit une requête move SANS bearer (pour les tests pré-auth 400/422).
fn move_req(id: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/v1/notes/{id}/move"))
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Construit une requête move AVEC bearer JWT.
fn move_req_authed(id: &str, body: serde_json::Value, token: &str) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/v1/notes/{id}/move"))
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

// S1.4-0 / D1.1 : move réussi → 204 + locus persisté ET lisible via vault (relocalisation).
#[tokio::test]
async fn move_success_persists_locus() {
    use gradatum_core::frontmatter::Frontmatter;
    use gradatum_core::scope::VaultId;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let (app, vault, token, _dir) = build_app_with_vault_and_token().await;

    // Écrire une vraie note (sans locus) via le Vault concret.
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Draft,
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
        .write_note(fm, "corps test move locus".into())
        .await
        .expect("write_note");
    let id = note.id.to_string();

    // Move via l'API HTTP — F-1 : bearer JWT obligatoire.
    let req = move_req_authed(
        &id,
        serde_json::json!({ "locus": "knowledge/rust" }),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // D1.1 : vault_read (read_note_by_id) doit retourner le NOUVEAU locus.
    let read = vault
        .read_note_by_id(&id)
        .await
        .expect("read_note_by_id après move");
    assert_eq!(
        read.frontmatter.locus.as_ref().map(|l| l.as_str()),
        Some("knowledge/rust"),
        "le move doit relocaliser le .md et exposer le nouveau locus via vault_read"
    );
}

// S1.4-1 : id non-ULID → 400.
#[tokio::test]
async fn move_bad_ulid_is_400() {
    let app = build_app().await;
    let req = move_req(
        "not-a-ulid",
        serde_json::json!({ "locus": "knowledge/rust" }),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// S1.4-2 : locus invalide (traversal, charset, slash terminal) → 400.
#[tokio::test]
async fn move_invalid_locus_is_400() {
    let app = build_app().await;
    let id = Ulid::new().to_string();
    for bad in ["../etc", "Knowledge", "a//b", "knowledge/", ""] {
        let req = move_req(&id, serde_json::json!({ "locus": bad }));
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "locus {bad:?} doit être rejeté en 400"
        );
    }
}

// S1.4-3 : note absente avec locus valide → 404 (après auth — bearer JWT requis).
#[tokio::test]
async fn move_unknown_note_is_404() {
    let (app, token) = build_app_with_token().await;
    let id = Ulid::new().to_string();
    let req = move_req_authed(
        &id,
        serde_json::json!({ "locus": "knowledge/rust" }),
        &token,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// S1.4-4 : champ inconnu dans le body → 422 (deny_unknown_fields à la désérialisation).
#[tokio::test]
async fn move_unknown_field_is_422() {
    let app = build_app().await;
    let id = Ulid::new().to_string();
    let req = move_req(&id, serde_json::json!({ "locus": "knowledge", "bogus": 1 }));
    let resp = app.oneshot(req).await.unwrap();
    // Axum renvoie 422 Unprocessable Entity sur échec de désérialisation Json.
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let _ = resp.into_body().collect().await;
}
