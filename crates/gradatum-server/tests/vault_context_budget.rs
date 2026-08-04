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
//!
//! **v0.7.0 — shape migration :** réponse `vault_context` passe de
//! `{ context, estimated_tokens, sources }` à
//! `{ assembled_text, budget_used, included, diagnostics }`.
//! `assembled_text` = ancien `context` (parité bit-pour-bit).
//! `budget_used` = ancien `estimated_tokens`.
//! `included[*].ulid` = ancien `sources[*]` (string ULID).

//! Couvre également :
//! 5. `budget_used_assembled_reflects_full_text_including_scaffolding` — P2-b :
//!    `budget_used` = estimation du texte assemblé complet (scaffolding inclus), pas seulement bodies.

#[path = "helpers/mod.rs"]
mod helpers;

use std::sync::Arc;

use helpers::{
    FakeEmbedder, build_app, build_app_with_embedder, call_vault_context, call_vault_context_json,
    sign_token,
};

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

    let assembled_text = resp["assembled_text"].as_str().expect("assembled_text str");
    let included = resp["included"].as_array().expect("included array");
    let budget_used = resp["budget_used"].as_u64().expect("budget_used u64") as u32;

    assert!(!assembled_text.is_empty(), "contexte vide — resp={resp}");
    assert!(!included.is_empty(), "included vide — resp={resp}");
    assert!(
        budget_used <= 500,
        "budget_used {} dépasse max_tokens 500",
        budget_used
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

    let assembled_text = resp["assembled_text"].as_str().expect("assembled_text str");
    let budget_used = resp["budget_used"].as_u64().expect("budget_used u64") as u32;

    let chars = assembled_text.chars().count();
    // 100 tokens × 3 chars = 300 chars max. Marge de 30 chars (séparateur "\n\n---\n\n"
    // potentiel + char_indices boundary unicode).
    assert!(
        chars <= 350,
        "contexte trop long : {chars} chars > 350 (budget 100 tokens × 3)",
    );
    // budget_used ≤ max_tokens (cohérence ratio 3.0 = chars / 3).
    assert!(
        budget_used <= 100,
        "budget_used {budget_used} dépasse max_tokens 100",
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

    let included = resp["included"].as_array().expect("included array");
    let assembled_text = resp["assembled_text"].as_str().expect("assembled_text str");

    // included[*].ulid contient l'ULID demandé.
    let source_ids: Vec<&str> = included.iter().filter_map(|n| n["ulid"].as_str()).collect();
    assert!(
        source_ids.contains(&nid.to_string().as_str()),
        "included doit contenir l'ULID demandé. source_ids={source_ids:?}"
    );
    assert!(
        assembled_text.contains("Contenu de la note directe"),
        "assembled_text doit inclure le body — assembled_text={assembled_text}"
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

    let assembled_text = resp["assembled_text"].as_str().expect("assembled_text str");
    assert!(
        assembled_text.contains("Décision relative au worker gradatum"),
        "assembled_text doit contenir la note decisions — assembled_text={assembled_text}"
    );
    assert!(
        !assembled_text.contains("Documentation de référence gradatum"),
        "assembled_text ne doit PAS contenir la note reference — assembled_text={assembled_text}"
    );
}

/// Test 5 : P2-b — `budget_used` en mode Assembled reflète le texte assemblé complet.
///
/// ## Propriété
///
/// `budget_used` doit être cohérent avec `estimator.estimate(&assembled_text)` (texte
/// complet = scaffolding `render_assembled` + bodies des notes), PAS avec la somme des
/// `estimate(body)` des notes seules.
///
/// ## TDD rouge → vert
///
/// - **Avant fix** : `budget_used = sum(estimate(body))` (select_budget_aware uniquement) —
///   inférieur à `estimate(assembled_text)` car le scaffolding Markdown est exclu.
///   La propriété `budget_used ≥ floor(assembled_chars/6)` ÉCHOUE.
/// - **Après fix** : `budget_used = estimator.estimate(&assembled_text)` — la propriété
///   est satisfaite bit-pour-bit.
#[tokio::test]
async fn budget_used_assembled_reflects_full_text_including_scaffolding() {
    // FakeEmbedder obligatoire : le chemin assembled repose sur embed pour le scoring
    // composite (non-Noop → query_embedding non-None → chemin complet activé).
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    // Note courte avec body connu — le scaffolding sera mesurable.
    env.write_note_with_h1("Note scaffold P2b", "alpha beta test scaffold p2b gradatum")
        .await;

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha",
            "mode": "assembled",
            "budget_tokens": 2000
        }),
    )
    .await;

    let assembled_text = resp["assembled_text"].as_str().expect("assembled_text str");
    let budget_used = resp["budget_used"].as_u64().expect("budget_used u64") as u32;

    // Le texte assemblé doit être non-vide (la note matche la requête "alpha").
    assert!(
        !assembled_text.is_empty(),
        "assembled_text ne doit pas être vide — la note matche 'alpha'"
    );

    // Propriété P2-b : budget_used est cohérent avec le texte assemblé complet.
    // HeuristicEstimator : words*1.3, plancher chars/6, plafond chars/2.
    // On vérifie que budget_used est dans les bornes de l'estimateur sur assembled_text.
    let assembled_chars = assembled_text.chars().count() as u64;
    let floor = (assembled_chars / 6).max(1);
    let ceil = (assembled_chars / 2).max(1);
    assert!(
        budget_used as u64 >= floor,
        "budget_used {budget_used} < plancher {floor} (assembled_chars/6 = {assembled_chars}/6). \
         Vérifier que budget_used = estimate(assembled_text) — pas sum(estimate(body))."
    );
    assert!(
        budget_used as u64 <= ceil,
        "budget_used {budget_used} > plafond {ceil} (assembled_chars/2 = {assembled_chars}/2)"
    );

    // Vérifier que le scaffolding est bien inclus dans assembled_text.
    // L'en-tête "Contexte assemblé pour" et les marqueurs "— source: [["
    // font partie du scaffolding render_assembled — ils ne sont PAS dans le body de la note.
    assert!(
        assembled_text.contains("Context assembled for"),
        "en-tête scaffolding absent de assembled_text — resp={resp}"
    );
    assert!(
        assembled_text.contains("— source: [["),
        "marqueur source absent de assembled_text — resp={resp}"
    );
}
