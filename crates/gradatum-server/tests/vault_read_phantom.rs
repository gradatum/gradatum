//! Test — `vault_read` renvoie 404 (pas 500) pour une note fantôme (.md absent).
//!
//! ## Couvre
//!
//! 1. `vault_read_phantom_note_returns_404` — note présente dans l'index SQLite mais
//!    fichier `.md` absent sur disque → HTTP 404 (et non 500).
//! 2. `vault_read_real_note_returns_200` — note avec `.md` + index → HTTP 200
//!    (non-régression).
//!
//! ## Racine du bug
//!
//! `read_note_by_id` propageait `GradatumError::Storage` (→ 500) quand la lecture du
//! fichier `.md` échouait avec absence (`StorageError::NotFound`, Display `not found:`).
//! Les 6 556 notes « fantômes » sont héritées de l'import legacy vault (2026-05-08) :
//! entrées présentes dans l'index SQLite mais sans fichier `.md` correspondant sur disque.
//!
//! ## Fix (D2 — typage à la source)
//!
//! `lifecycle::read_note` détecte `StorageError::NotFound` et remonte un
//! `VaultError::Core(GradatumError::NoteNotFound(id))` TYPÉ. Celui-ci traverse
//! `read_note_by_id` (`Core(inner) => inner`) intact, si bien que TOUS les appelants
//! (`vault_read`, `vault_classify`, `reads`, RMW) répondent 404 via `err_to_status` —
//! sans aucun string-match fragile sur `msg.contains("not found:")`.

#[path = "helpers/mod.rs"]
mod helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use helpers::{build_app, call_vault_read, seed_note_sql_only, sign_token};
use tower::ServiceExt;
use ulid::Ulid;

/// vault_read renvoie 404 pour une note fantôme (.md absent, index présent).
///
/// `seed_note_sql_only` insère l'entrée dans l'index SQLite SANS écrire le fichier `.md` —
/// ce qui simule exactement les notes importées depuis legacy vault sans fichier correspondant.
/// Avant le fix, `read_note_by_id` remontait `GradatumError::Storage("read .md …:
/// not found: …")` → `err_to_status` → HTTP 500.
/// Après le fix (D2), `read_note` remonte un `NoteNotFound` typé → HTTP 404.
#[tokio::test]
async fn vault_read_phantom_note_returns_404() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Insère dans l'index SQLite SANS créer le fichier .md — note fantôme.
    // `env._vault_typed.index()` retourne `&Arc<SqliteIndex>` ; `.as_ref()` déréférence
    // l'Arc en `&SqliteIndex` attendu par `seed_note_sql_only`.
    let phantom_ulid = Ulid::new().to_string();
    seed_note_sql_only(
        env._vault_typed.index().as_ref(),
        &phantom_ulid,
        "reference",
        "Note Fantôme Test 404",
        "Corps de test fantôme.",
    )
    .await;

    let result = call_vault_read(env.app.clone(), &token, &phantom_ulid, "main").await;

    assert_eq!(
        result.unwrap_err(),
        StatusCode::NOT_FOUND,
        "vault_read doit renvoyer 404 pour une note fantôme (.md absent), \
         jamais 500 (Storage). phantom_ulid={phantom_ulid}"
    );
}

/// vault_read renvoie 200 pour une note réelle (non-régression).
///
/// `write_note_with_h1` écrit le fichier `.md` sur disque ET insère dans l'index.
/// Le comportement normal doit être préservé après l'ajout du bras fantôme dans
/// `read_note_impl`.
#[tokio::test]
async fn vault_read_real_note_returns_200() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let note_id = env
        .write_note_with_h1(
            "Note Réelle Non Régression",
            "Contenu réel présent sur disque.",
        )
        .await;

    let result = call_vault_read(env.app.clone(), &token, &note_id.to_string(), "main").await;

    assert!(
        result.is_ok(),
        "vault_read doit renvoyer 200 pour une note réelle (.md présent). result={result:?}"
    );
}

/// vault_classify renvoie 404 pour une note fantôme (.md absent, index présent).
///
/// TDD D2 : avant le typage à la source, `vault_classify_impl` propageait via `?`
/// le `GradatumError::Storage("read .md …: not found: …")` → `err_to_status` → HTTP 500.
/// Après le fix, `read_note_by_id` remonte un `NoteNotFound` typé → HTTP 404.
/// Réutilise `seed_note_sql_only` (même fixture que `vault_read_phantom_note_returns_404`).
#[tokio::test]
async fn vault_classify_phantom_note_returns_404() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let phantom_ulid = Ulid::new().to_string();
    seed_note_sql_only(
        env._vault_typed.index().as_ref(),
        &phantom_ulid,
        "reference",
        "Note Fantôme Classify 404",
        "Corps de test fantôme classify.",
    )
    .await;

    let body = serde_json::json!({ "note_id": phantom_ulid, "tenant_id": "main" });
    let req = Request::builder()
        .uri("/api/v1/vault_classify")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&body).expect("serialize classify body"),
        ))
        .expect("build vault_classify request");

    let resp = env
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("vault_classify oneshot");

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "vault_classify doit renvoyer 404 pour une note fantôme (.md absent), \
         jamais 500 (Storage). phantom_ulid={phantom_ulid}"
    );
}
