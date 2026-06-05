//! Tests E2E M4 — `vault_trace` accepte une query textuelle (Task 15 alpha.13).
//!
//! Résolution multi-mode rev2 §2.2 :
//! 1. ULID direct → `trace_lineage` immédiat (non-régression)
//! 2. Titre exact → `title_lookup` → `trace_lineage`
//! 3. Query textuelle → `search_fts_with_snippet` → `trace_lineage` multi-seeds
//!
//! Couvre 4 cas (cf. spec rev2.1 §4 Task 15 + ajout test title) :
//! 1. `vault_trace_accepts_text_query_and_returns_trace_entries` — FTS textuel
//! 2. `vault_trace_ulid_still_works_after_m4_patch` — non-régression ULID
//! 3. `vault_trace_text_query_no_match_returns_empty` — query inconnue → []
//! 4. `vault_trace_resolves_title_to_lineage` — titre exact → trace_lineage

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{build_app, call_vault_trace, sign_token};

/// Test 1 : query textuelle (non-ULID, pas un titre exact) → FTS multi-seeds.
///
/// Seed deux notes A et B reliées par un wikilink (B → A). La query "architecture
/// gradatum" matche A en BM25. trace_lineage(A) doit retourner B comme parent.
#[tokio::test]
async fn vault_trace_accepts_text_query_and_returns_trace_entries() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    // Note A : "Architecture Gradatum" (cible)
    let id_a = env
        .write_note_with_h1(
            "Architecture Gradatum",
            "Description complète de l'architecture du système.",
        )
        .await;
    // Note B : pointe vers A via wikilink
    let id_b = env
        .write_note_with_h1(
            "Notes Voisines XYZ",
            "Document lié à l'architecture du projet.",
        )
        .await;
    // Persistance manuelle du lien (pas de pipeline curate complet ici)
    env.state
        .search
        .upsert_link("main", &id_b.to_string(), &id_a.to_string())
        .await
        .expect("upsert_link B → A");

    let resp = call_vault_trace(env.app.clone(), &token, "architecture", "main", 10)
        .await
        .expect("vault_trace textuel doit réussir");

    let entries = resp["entries"].as_array().expect("entries array");
    // Au moins une entrée (B est parent de A via le lien upsert-é).
    assert!(
        !entries.is_empty(),
        "vault_trace textuel retourne 0 entrées — resp={resp}"
    );
}

/// Test 2 : non-régression — `vault_trace` par ULID direct doit toujours fonctionner.
#[tokio::test]
async fn vault_trace_ulid_still_works_after_m4_patch() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let id_a = env.write_note_with_h1("Note A", "Contenu A").await;
    let id_b = env.write_note_with_h1("Note B", "Contenu B").await;
    env.state
        .search
        .upsert_link("main", &id_a.to_string(), &id_b.to_string())
        .await
        .expect("upsert_link A → B");

    let resp = call_vault_trace(env.app.clone(), &token, &id_a.to_string(), "main", 10)
        .await
        .expect("vault_trace ULID doit réussir");

    let entries = resp["entries"].as_array().expect("entries array");
    let paths: Vec<&str> = entries.iter().filter_map(|e| e["path"].as_str()).collect();
    assert!(
        paths.iter().any(|p| p.contains(&id_b.to_string())),
        "enfant id_b manquant dans vault_trace ULID — paths={paths:?}"
    );
}

/// Test 3 : query textuelle sans match → entries vide (pas d'erreur 500).
#[tokio::test]
async fn vault_trace_text_query_no_match_returns_empty() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let resp = call_vault_trace(
        env.app.clone(),
        &token,
        "requete totalement inexistante xyzzy",
        "main",
        10,
    )
    .await
    .expect("vault_trace sans match doit réussir HTTP 200");

    let entries = resp["entries"].as_array().expect("entries array");
    assert!(
        entries.is_empty(),
        "vault_trace sans match doit retourner [] — resp={resp}"
    );
}

/// Test 4 : titre exact (ni ULID, ni query FTS) → résolu via `title_lookup` → lineage.
#[tokio::test]
async fn vault_trace_resolves_title_to_lineage() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let id_root = env
        .write_note_with_h1("Mon Titre Exact", "body root note")
        .await;
    let id_child = env.write_note_with_h1("Note Enfant Y", "lien enfant").await;
    env.state
        .search
        .upsert_link("main", &id_root.to_string(), &id_child.to_string())
        .await
        .expect("upsert_link root → child");

    let resp = call_vault_trace(env.app.clone(), &token, "Mon Titre Exact", "main", 10)
        .await
        .expect("vault_trace titre exact doit réussir");

    let entries = resp["entries"].as_array().expect("entries array");
    let paths: Vec<&str> = entries.iter().filter_map(|e| e["path"].as_str()).collect();
    assert!(
        paths.iter().any(|p| p.contains(&id_child.to_string())),
        "enfant via titre exact manquant — paths={paths:?}"
    );
}
