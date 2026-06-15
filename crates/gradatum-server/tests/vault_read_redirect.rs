//! Tests F-39 — fallback redirect en couche lecture (`vault_read`).
//!
//! Valide que `vault_read` résout transparentement un ancien titre via
//! `redirect_table` quand `title_lookup` échoue (note renommée).
//!
//! ## Cas couverts
//!
//! 1. `vault_read_falls_back_to_redirect_after_rename`
//!    — titre renommé → title_lookup échoue → resolve_redirect → 200 OK.
//! 2. `vault_read_redirect_unknown_slug_returns_404`
//!    — slug inconnu dans redirect_table → 404 (non-régression).
//!
//! ## Approche fixture
//!
//! Pour forcer le fallback redirect (et non le path title_lookup normal), on seed
//! la note avec body_text = `# Nouveau Titre\n...` (titre déjà renommé) mais on
//! enregistre un redirect `slug("Ancien Titre") → ULID`. Ainsi `title_lookup("Ancien
//! Titre")` ne trouve rien (body a le nouveau titre), mais `resolve_redirect` réussit.

#[path = "helpers/mod.rs"]
mod helpers;

use axum::http::StatusCode;
use gradatum_index::links::title_to_slug;

use helpers::{build_app, call_vault_read, call_vault_read_raw, sign_token};

/// Test 1 : après rename d'une note, `vault_read(ancien_titre)` résout via redirect.
///
/// Scénario :
/// 1. Seed note avec titre courant "Nouveau Titre" dans le vault (fichier .md + index).
/// 2. Enregistrer un redirect `slug("Ancien Titre") → ULID` dans la redirect_table
///    (simule un rename antérieur : "Ancien Titre" → "Nouveau Titre").
/// 3. `vault_read("Ancien Titre")` — title_lookup ne trouve pas ce titre (body a
///    "Nouveau Titre"), mais resolve_redirect retourne le bon ULID → 200 OK.
#[tokio::test]
async fn vault_read_falls_back_to_redirect_after_rename() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Seed la note avec le titre COURANT (après rename)
    // body_text = "# Nouveau Titre\n..." → title_lookup("Ancien Titre") ne trouve rien.
    let nid = env
        .write_note_with_h1("Nouveau Titre", "Contenu après renommage de la note.")
        .await;

    // Enregistrer le redirect : slug("Ancien Titre") → ULID de "Nouveau Titre"
    // Simule ce que `gradatum-admin vault rename "Ancien Titre" "Nouveau Titre"` ferait.
    let slug = title_to_slug("Ancien Titre");
    env.state
        .search
        .upsert_redirect(&slug, &nid.0, chrono::Utc::now().timestamp_millis())
        .await
        .expect("upsert_redirect simulation rename");

    // Vérification préalable : title_lookup direct ne trouve PAS "Ancien Titre"
    let direct = env
        .state
        .search
        .title_lookup("main", "Ancien Titre")
        .await
        .expect("title_lookup ne doit pas échouer");
    assert!(
        direct.is_none(),
        "title_lookup('Ancien Titre') doit retourner None — le body contient 'Nouveau Titre'"
    );

    // vault_read par l'ancien titre doit réussir via redirect (F-39 fallback)
    let resp = call_vault_read(env.app.clone(), &token, "Ancien Titre", "main")
        .await
        .expect("vault_read ancien titre doit réussir via redirect F-39");

    assert_eq!(
        resp["path"].as_str(),
        Some(nid.to_string().as_str()),
        "le path retourné doit être l'ULID de la note renommée. resp={resp}"
    );
}

/// Test 2 : slug inconnu dans redirect_table → 404 (non-régression).
///
/// Si ni title_lookup ni resolve_redirect ne trouvent rien → 404.
#[tokio::test]
async fn vault_read_redirect_unknown_slug_returns_404() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Aucune note avec ce titre, aucun redirect enregistré
    let resp = call_vault_read_raw(
        env.app.clone(),
        &token,
        "Titre Jamais Vu XYZ Redirect",
        "main",
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "slug inconnu → 404 même avec le fallback redirect"
    );
}
