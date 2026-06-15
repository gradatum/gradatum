//! Tests E2E `POST /api/v1/notes/{id}/move` — move to locus (F-37 S1.4 + D1.1).
//!
//! Depuis D1.1 (v0.4.8), le move est une **relocalisation physique** du `.md` via le
//! chemin Vault (`vault.move_locus`), cohérente de bout en bout (`.md` + index + CoW).
//! Couvre :
//! 0. `move_success_persists_locus` — 204 + locus persisté (vérifié via `vault_read`).
//! 1. `move_bad_ulid_is_400` — id non-ULID rejeté.
//! 2. `move_invalid_locus_is_400` — locus charset/traversal/longueur invalide.
//! 3. `move_unknown_note_is_404` — note absente.
//! 4. `move_unknown_field_is_422` — body avec champ inconnu (deny_unknown_fields).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_vault::{Registry, Vault};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use ulid::Ulid;

/// Construit une app de test avec un Vault réel (TempDir). Retourne le handle concret
/// `Arc<Vault>` (pour écrire et relire des notes) ; le même `Arc` est wiré comme
/// `Arc<dyn Registry>` dans l'état, donc l'app et le test partagent le même Vault.
///
/// Le `TempDir` est retourné pour maintenir le répertoire vivant pendant le test.
async fn build_app_with_vault() -> (axum::Router, Arc<Vault>, TempDir) {
    use axum::{middleware, Router};
    use gradatum_core::scope::VaultId;
    use gradatum_server::state::AppState;

    let dir = TempDir::new().expect("TempDir");
    let vault = Arc::new(
        Vault::create(dir.path(), VaultId::new("main"))
            .await
            .expect("Vault::create"),
    );

    let state = AppState::new().with_vault_arc(Arc::clone(&vault) as Arc<dyn Registry>);

    let app = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state.clone());

    (app, vault, dir)
}

/// App minimale sans Vault réel (PlaceholderRegistry) — suffisante pour les cas
/// d'erreur qui ne touchent pas une note réelle (400/404/422).
async fn build_app() -> axum::Router {
    use axum::{middleware, Router};
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

fn move_req(id: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .uri(format!("/api/v1/notes/{id}/move"))
        .method("POST")
        .header("content-type", "application/json")
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

    let (app, vault, _dir) = build_app_with_vault().await;

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

    // Move via l'API HTTP — passe désormais par vault.move_locus (relocalisation physique).
    let req = move_req(&id, serde_json::json!({ "locus": "knowledge/rust" }));
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

// S1.4-3 : note absente (PlaceholderRegistry) avec locus valide → 404.
#[tokio::test]
async fn move_unknown_note_is_404() {
    let app = build_app().await;
    let id = Ulid::new().to_string();
    let req = move_req(&id, serde_json::json!({ "locus": "knowledge/rust" }));
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
