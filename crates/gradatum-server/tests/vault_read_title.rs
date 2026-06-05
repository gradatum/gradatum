//! Tests E2E B4 — `vault_read` accepte un titre H1 comme `path` (Task 14 alpha.13).
//!
//! Décision rev2 P0-3 Option A : intégration `title_lookup` directement
//! dans le handler `vault_read` (pas de handler dédié). Cohérent DTO `SearchHit.title`
//! alpha.11-patch.1.
//!
//! Couvre 4 cas (cf. spec rev2.1 §4 Task 14 Step 1) :
//! 1. `vault_read_accepts_title_as_path` — résolution titre → ULID
//! 2. `vault_read_title_not_found_returns_404` — titre inexistant → 404
//! 3. `vault_read_ulid_still_works_after_b4_patch` — non-régression ULID
//! 4. `vault_read_does_not_resolve_downgraded_note_by_title` — filtre `status = 'live'`
//!
//! Spec : docs/specs/2026-05-10-phase-2x4-alpha-13-endpoints-completeness-spec-rev2.md
//! Pré-vérif C7-bis : LIVE backfill `title` quasi-vide (1/552) → conserver `LIKE
//! body_text` côté `title_lookup`. Le filtre `status = 'live'` reste à ajouter.

#[path = "helpers/mod.rs"]
mod helpers;

use axum::http::StatusCode;

use helpers::{build_app, call_vault_read, call_vault_read_raw, sign_token};

/// Test 1 : `vault_read({path: "Mon Architecture"})` résout via `title_lookup`.
///
/// Le `path` retourné dans la réponse JSON doit être l'ULID résolu (pas le titre).
#[tokio::test]
async fn vault_read_accepts_title_as_path() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let nid = env
        .write_note_with_h1(
            "Mon Architecture",
            "Description complète de l'architecture.",
        )
        .await;

    let resp = call_vault_read(env.app.clone(), &token, "Mon Architecture", "main")
        .await
        .expect("vault_read par titre doit réussir");

    assert_eq!(
        resp["path"].as_str(),
        Some(nid.to_string().as_str()),
        "path doit être l'ULID résolu, pas le titre. resp={resp}"
    );
    assert!(
        resp["content"]
            .as_str()
            .unwrap_or("")
            .contains("Mon Architecture"),
        "content doit inclure le H1. resp={resp}"
    );
}

/// Test 2 : titre inexistant → 404 NOT_FOUND.
#[tokio::test]
async fn vault_read_title_not_found_returns_404() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let resp = call_vault_read_raw(env.app.clone(), &token, "Titre Inexistant XYZ", "main").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "titre inconnu → 404");
}

/// Test 3 : non-régression — `vault_read` par ULID direct doit toujours fonctionner.
#[tokio::test]
async fn vault_read_ulid_still_works_after_b4_patch() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let nid = env
        .write_note_with_h1("Note ULID", "Contenu de la note ULID.")
        .await;

    let resp = call_vault_read(env.app.clone(), &token, &nid.to_string(), "main")
        .await
        .expect("vault_read par ULID doit réussir");
    assert_eq!(
        resp["path"].as_str(),
        Some(nid.to_string().as_str()),
        "path retourné = ULID demandé. resp={resp}"
    );
}

/// Test 4 : note `status = 'downgraded'` ne doit PAS être résolue par titre.
///
/// Filtre `AND status = 'live'` ajouté dans `title_lookup` (rev2 §2.1) — distinct
/// du concept `include_downgraded` (FTS5 highlight, voir caveat C6-bis).
#[tokio::test]
async fn vault_read_does_not_resolve_downgraded_note_by_title() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Seed via vault → fichier .md OK → puis downgrade via search.downgrade_note.
    let _nid = env.write_note_downgraded("Note Archivée").await;

    let resp = call_vault_read_raw(env.app.clone(), &token, "Note Archivée", "main").await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "note downgraded ne doit pas être résoluble par titre"
    );
}
