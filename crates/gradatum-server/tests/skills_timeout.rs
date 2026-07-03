//! Tests TDD P2-a — Timeout sur l'embed des skills (`build_skill_index`).
//!
//! # Propriété testée
//!
//! Quand `embed_timeout_ms` est inférieur au délai de `embed_batch` dans
//! `build_skill_index`, l'index skills doit être vide (dégradation gracieuse)
//! plutôt que de bloquer le write lock de `skills_index`.
//!
//! # Embedder différencié (`SlowBatchEmbedder`)
//!
//! - `embed` (requête principale, retrieval) → rapide → `query_embedding = Some(...)`.
//! - `embed_batch` (index skills, `build_skill_index`) → lent → déclenche le timeout.
//!
//! Sans cet embedder différencié, le timeout de retrieval (même paramètre
//! `embed_timeout_ms`) masquerait le timeout de `build_skill_index` car
//! `query_embedding = None` → injection skippée sans jamais appeler `build_skill_index`.

#[path = "helpers/mod.rs"]
mod helpers;

use std::sync::Arc;

use helpers::{
    SlowBatchEmbedder, build_app_with_context_config, call_vault_context_json, seed_notes,
    seed_skill, sign_token,
};

/// Avec `embed_timeout_ms = 1ms` et `SlowBatchEmbedder(batch_delay=200ms)` :
/// - `retrieve_candidates` : `embed` (rapide) → `query_embedding = Some(...)` → retrieval réussi.
/// - `build_skill_index` : `embed_batch` (200ms) → timeout 1ms → index vide.
/// → `skills_injected = 0`, pas de panic, pas d'erreur HTTP.
///
/// # TDD rouge → vert
///
/// - **Avant fix** : `build_skill_index` ignore `embed_timeout_ms` (paramètre absent) →
///   `embed_batch` attend 200ms et réussit → index construit → `skills_injected ≥ 1` → FAIL.
/// - **Après fix** : `embed_timeout_ms = 1ms` déclenché sur `embed_batch` (200ms) →
///   `SkillIndex { entries: [] }` → `skills_injected = 0` → PASS.
#[tokio::test]
async fn build_skill_index_embed_timeout_returns_empty_index_no_panic() {
    let env = build_app_with_context_config(
        // SlowBatchEmbedder : embed rapide (retrieval OK) + embed_batch lent (timeout skills).
        Arc::new(SlowBatchEmbedder {
            batch_delay_ms: 200,
        }),
        // Timeout 1ms << 200ms délai embed_batch → timeout skills garanti.
        // Timeout retrieval (embed) : embed rapide (0ms) < 1ms → retrieval embed réussit.
        gradatum_server::config::ContextConfig {
            embed_timeout_ms: 1,
            ..Default::default()
        },
    )
    .await;

    // Seeder un skill : sans timeout sur build_skill_index, il serait embeddé et injecté.
    seed_skill(&env, "Skill lent", "contenu skill embed lent alpha").await;
    // Seeder des notes qui matchent "alpha" via FTS → candidats non-vides
    // (condition nécessaire : l'injection skills ne se déclenche que si candidates != []).
    seed_notes(&env, 3).await;

    let token = sign_token(&env.state);
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha",
            "mode": "assembled",
            "inject_skills": true
        }),
    )
    .await;

    // Propriété P2-a : timeout embed_batch → index vide → aucune injection, pas de panic.
    assert_eq!(
        resp["diagnostics"]["skills_injected"],
        serde_json::json!(0),
        "skills_injected doit être 0 sur timeout embed_batch (SlowBatchEmbedder 200ms > timeout 1ms). \
         diagnostics={}",
        resp["diagnostics"]
    );
    // La réponse doit être valide (pas d'erreur HTTP 500 — dégradation gracieuse).
    assert!(
        resp["assembled_text"].is_string(),
        "assembled_text doit être présent même sur timeout skills — resp={resp}"
    );
}
