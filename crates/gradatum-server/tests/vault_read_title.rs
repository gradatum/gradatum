//! Tests E2E B4 — `vault_read` accepte un titre H1 comme `path`.
//!
//! `title_lookup` est intégré directement dans le handler `vault_read`.
//!
//! Couvre 4 cas :
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

/// Test 5 (item B) : `vault_read` accepte la forme `section/ulid` émise par `vault_search`.
///
/// Round-trip search→read : `vault_search` émet `path = "decisions/<ulid>"` mais l'ancien
/// handler parseait `Ulid::from_string("decisions/<ulid>")` → Err → title_lookup → 404.
///
/// Ce test vérifie que `vault_read(path="decisions/<ulid>")` retourne 200 et le même
/// contenu que `vault_read(path="<ulid>")`.
#[tokio::test]
async fn vault_read_accepts_section_prefixed_path() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Seed via Vault::write_note — fichier .md sur disque + index SQLite.
    let nid = env
        .write_note_in_section("decisions", "Note Section Prefixed", "Contenu item B.")
        .await;

    let ulid_str = nid.to_string();
    let prefixed = format!("decisions/{ulid_str}");

    // Lecture par ULID nu — doit fonctionner (non-régression).
    let by_bare = call_vault_read(env.app.clone(), &token, &ulid_str, "main")
        .await
        .expect("vault_read par ULID nu doit réussir");

    // Lecture par section/ulid — c'est le cas cassé qu'on corrige.
    let by_prefixed = call_vault_read(env.app.clone(), &token, &prefixed, "main")
        .await
        .expect("vault_read par section/ulid doit réussir (item B)");

    assert_eq!(
        by_bare["path"].as_str(),
        by_prefixed["path"].as_str(),
        "le path retourné doit être le même ULID dans les deux cas"
    );
    assert_eq!(
        by_bare["content"].as_str(),
        by_prefixed["content"].as_str(),
        "le contenu doit être identique quelle que soit la forme du path"
    );
}
