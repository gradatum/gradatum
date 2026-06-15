//! Tests E2E M5 — `vault_context` budget tokens + FTS multi-notes.
//!
//! Heuristique tokens :
//! - `chars().count()` (pas `len()` bytes) — cohérence unicode FR/EN/multi-byte
//! - Ratio **3.0 chars/token** — conservateur FR/EN mixte
//!
//! Couvre 4 cas :
//! 1. `vault_context_text_query_returns_multiple_notes_within_budget` — FTS multi-notes
//! 2. `vault_context_respects_max_tokens_budget` — troncature char-safe
//! 3. `vault_context_ulid_direct_still_works` — non-régression ULID
//! 4. `vault_context_section_filter_applies` — filtre section FTS

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{build_app, call_vault_context, sign_token};

/// Test 1 : query textuelle → multi-notes agrégées dans budget.
#[tokio::test]
async fn vault_context_text_query_returns_multiple_notes_within_budget() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    env.write_note_with_h1("Note A", "Architecture gradatum agents alpha13.")
        .await;
    env.write_note_with_h1("Note B", "Gradatum worker dispatch alpha13.")
        .await;
    env.write_note_with_h1("Note C Hors Sujet", "tout autre chose ici, rien à voir.")
        .await;

    let resp = call_vault_context(
        env.app.clone(),
        &token,
        "gradatum agents",
        "main",
        Some(500),
        None,
    )
    .await
    .expect("vault_context doit réussir");

    let context = resp["context"].as_str().expect("context str");
    let sources = resp["sources"].as_array().expect("sources array");
    let est_tokens = resp["estimated_tokens"]
        .as_u64()
        .expect("estimated_tokens u64") as u32;

    assert!(!context.is_empty(), "contexte vide — resp={resp}");
    assert!(!sources.is_empty(), "sources vides — resp={resp}");
    assert!(
        est_tokens <= 500,
        "estimated_tokens {} dépasse max_tokens 500",
        est_tokens
    );
}

/// Test 2 : note longue tronquée à `max_tokens × 3` chars (ratio 3.0 rev2).
#[tokio::test]
async fn vault_context_respects_max_tokens_budget() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let long_body = "x".repeat(10_000);
    env.write_note_with_h1("Note Longue", &long_body).await;

    let resp = call_vault_context(env.app.clone(), &token, "Longue", "main", Some(100), None)
        .await
        .expect("vault_context doit réussir");

    let context = resp["context"].as_str().expect("context str");
    let est_tokens = resp["estimated_tokens"]
        .as_u64()
        .expect("estimated_tokens u64") as u32;

    let chars = context.chars().count();
    // 100 tokens × 3 chars = 300 chars max. Marge de 30 chars (séparateur "\n\n---\n\n"
    // potentiel + char_indices boundary unicode).
    assert!(
        chars <= 350,
        "contexte trop long : {chars} chars > 350 (budget 100 tokens × 3)",
    );
    // estimated_tokens ≤ max_tokens (cohérence ratio 3.0 = chars / 3).
    assert!(
        est_tokens <= 100,
        "estimated_tokens {est_tokens} dépasse max_tokens 100",
    );
}

/// Test 3 : non-régression — `req.query` ULID direct retourne note + backlinks.
#[tokio::test]
async fn vault_context_ulid_direct_still_works() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    let nid = env
        .write_note_with_h1("Note Directe", "Contenu de la note directe.")
        .await;

    let resp = call_vault_context(
        env.app.clone(),
        &token,
        &nid.to_string(),
        "main",
        None,
        None,
    )
    .await
    .expect("vault_context ULID doit réussir");

    let sources = resp["sources"].as_array().expect("sources array");
    let context = resp["context"].as_str().expect("context str");

    let source_ids: Vec<&str> = sources.iter().filter_map(|s| s.as_str()).collect();
    assert!(
        source_ids.contains(&nid.to_string().as_str()),
        "sources doit contenir l'ULID demandé. sources={source_ids:?}"
    );
    assert!(
        context.contains("Contenu de la note directe"),
        "context doit inclure le body — context={context}"
    );
}

/// Test 4 : filtre section FTS appliqué.
///
/// Note "Decisions Gradatum" en section `decisions`, note "Reference Gradatum" en
/// section `reference`. Query "Gradatum" + section=`decisions` → contexte ne doit
/// contenir QUE la note decisions (le filtre section est passé à `search_fts_with_snippet`).
#[tokio::test]
async fn vault_context_section_filter_applies() {
    let env = build_app().await;
    let token = sign_token(&env.state);

    env.write_note_in_section(
        "decisions",
        "Decision Gradatum One",
        "Décision relative au worker gradatum.",
    )
    .await;
    env.write_note_in_section(
        "reference",
        "Reference Gradatum Two",
        "Documentation de référence gradatum.",
    )
    .await;

    let resp = call_vault_context(
        env.app.clone(),
        &token,
        "gradatum",
        "main",
        Some(2000),
        Some("decisions"),
    )
    .await
    .expect("vault_context filtré section doit réussir");

    let context = resp["context"].as_str().expect("context str");
    assert!(
        context.contains("Décision relative au worker gradatum"),
        "context doit contenir la note decisions — context={context}"
    );
    assert!(
        !context.contains("Documentation de référence gradatum"),
        "context ne doit PAS contenir la note reference — context={context}"
    );
}
