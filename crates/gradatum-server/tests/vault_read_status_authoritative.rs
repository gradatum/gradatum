//! Test — `vault_read` renvoie le statut DB autoritatif, pas le frontmatter périmé.
//!
//! Couvre :
//! 1. `vault_read_returns_db_status_when_downgraded` — note DB=downgraded, frontmatter=live →
//!    `metadata.status == "downgraded"` (et non "live").
//! 2. `vault_read_returns_live_status_for_live_note` — note DB=live, frontmatter=live →
//!    `metadata.status == "live"` (non-régression).
//!
//! Racine du bug : `vault_downgrade` met à jour `notes.status` en DB mais PAS le
//! frontmatter YAML dans le fichier .md. `read_note_impl` lisait `note.frontmatter.status`
//! (stale) au lieu de la colonne DB (authoritative).
//!
//! Fix : appel `state.search.get_statuses(&tenant, &[note_id])` dans `read_note_impl` —
//! valeur DB si présente, sinon fallback frontmatter (dégradation gracieuse).

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{build_app, call_vault_read, sign_token};

/// Test 1 : `vault_read` retourne le statut DB autoritatif pour une note downgradée.
///
/// La note est créée avec `frontmatter.status = Live` ("live"), puis downgradée via
/// `downgrade_note` qui met à jour uniquement la colonne `notes.status` en DB.
/// Le frontmatter reste "live" dans le fichier .md sur disque.
/// `vault_read` DOIT retourner `"downgraded"` (DB), pas `"live"` (frontmatter stale).
#[tokio::test]
async fn vault_read_returns_db_status_when_downgraded() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // write_note_downgraded : frontmatter.status=Live ("live") → downgrade_note →
    // DB notes.status="downgraded". Frontmatter sur disque reste "live" (stale).
    let nid = env
        .write_note_downgraded("Note Downgradée Statut Autoritatif")
        .await;

    let resp = call_vault_read(env.app.clone(), &token, &nid.to_string(), "main")
        .await
        .expect("vault_read doit réussir sur une note downgradée adressée par ULID");

    assert_eq!(
        resp["metadata"]["status"].as_str(),
        Some("downgraded"),
        "vault_read doit renvoyer le statut DB (downgraded), pas le frontmatter stale (live). resp={resp}"
    );
}

/// Test 2 (non-régression) : `vault_read` retourne `"live"` pour une note live.
///
/// Note créée avec `frontmatter.status = Live` + aucun downgrade → colonne DB et
/// frontmatter cohérents → `metadata.status == "live"` dans les deux chemins.
#[tokio::test]
async fn vault_read_returns_live_status_for_live_note() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let nid = env
        .write_note_with_h1(
            "Note Live Statut Non-Régression",
            "Contenu non-régression statut live.",
        )
        .await;

    let resp = call_vault_read(env.app.clone(), &token, &nid.to_string(), "main")
        .await
        .expect("vault_read doit réussir sur une note live");

    assert_eq!(
        resp["metadata"]["status"].as_str(),
        Some("live"),
        "vault_read doit renvoyer 'live' pour une note live. resp={resp}"
    );
}
