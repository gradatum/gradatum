//! Tests d'intégration — F-58 : injection de skills dans `vault_context` assembled.
//!
//! # Pattern TDD
//!
//! Ces tests sont écrits EN PREMIER (rouge) puis rendus verts par l'implémentation.
//!
//! # Pourquoi FakeEmbedder obligatoire pour le test positif
//!
//! Sous `NoopBackend`, `backend_kind() == Noop` → `query_embedding = None`
//! dans `RetrievalOutcome` → injection skippée silencieusement (spec F-58).
//! `FakeEmbedder` (`backend_kind = Http`) active le chemin sémantique :
//! l'embedding de la requête est calculé → index skills buildable → injection active.

#[path = "helpers/mod.rs"]
mod helpers;

use std::sync::Arc;

use helpers::{
    FakeEmbedder, build_app, build_app_with_embedder, call_vault_context_json, seed_notes,
    seed_skill, sign_token,
};

/// `inject_skills` absent du payload → défaut `false` → zéro coût, `skills_injected == 0`.
///
/// Vérifie que le champ optionnel est bien `false` par défaut (rétrocompat) et que
/// l'absence de l'option ne déclenche aucun scan d'index ni appel embed supplémentaire.
#[tokio::test]
async fn inject_skills_off_by_default_zero_cost() {
    let env = build_app().await;
    seed_notes(&env, 5).await;
    let token = sign_token(&env.state);
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha",
            "mode": "assembled"
            // inject_skills absent → false par défaut (#[serde(default)])
        }),
    )
    .await;
    assert_eq!(
        resp["diagnostics"]["skills_injected"],
        serde_json::json!(0),
        "inject_skills absent doit retourner skills_injected=0, got: {}",
        resp["diagnostics"]["skills_injected"]
    );
}

/// `inject_skills=true` + FakeEmbedder → skill seedé visible dans `assembled_text`
/// et `skills_injected ≥ 1`.
///
/// ## Pourquoi la requête "alpha" et non "comment déployer"
///
/// L'injection de skills ne se déclenche QUE si `candidates` est non-vide (le retour
/// anticipé sur `candidates.is_empty()` précède l'injection — spec F-58). La requête
/// "alpha" matche les notes seedées par `seed_notes` ("alpha beta contenu…") via FTS,
/// garantissant que `candidates` est non-vide. Le skill ("Déploiement") est ensuite
/// injecté en tête via cosine ranking sur `query_embedding`, indépendamment du fait
/// que sa section `"skills"` ne soit pas dans les candidats de contexte.
///
/// Vérifie la chaîne complète :
/// 1. `seed_skill` écrit une note section `"skills"` dans l'index (SQL only).
/// 2. `seed_notes` seede 5 notes section `"reference"` matchant "alpha" via FTS.
/// 3. `vault_context` avec `inject_skills=true` : retrieval trouve des candidats (step 2),
///    puis déclenche le lazy build de l'index skills (step 1).
/// 4. `rank_skills` classe le skill en tête par cosine (FakeEmbedder déterministe).
/// 5. `inject_skills_header` prépend le bloc Markdown au texte assemblé.
/// 6. `diagnostics.skills_injected ≥ 1` et `assembled_text` contient "checklist deploy".
#[tokio::test]
async fn inject_skills_on_surfaces_relevant_skill() {
    // P0-1 : FakeEmbedder obligatoire — sous Noop, query_embedding=None → skip silencieux.
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    seed_skill(&env, "Déploiement", "checklist deploy LIVE").await;
    seed_notes(&env, 5).await; // notes "alpha beta" — matchent la requête "alpha"
    let token = sign_token(&env.state);
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha",             // matche les notes seed_notes via FTS
            "mode": "assembled",
            "inject_skills": true,
            "skill_query": "deploy"       // accepté, ignoré pour le ranking (F-58 zéro embed)
        }),
    )
    .await;
    assert!(
        resp["diagnostics"]["skills_injected"].as_u64().unwrap() >= 1,
        "skills_injected doit être ≥ 1, got: {}",
        resp["diagnostics"]["skills_injected"]
    );
    assert!(
        resp["assembled_text"]
            .as_str()
            .unwrap()
            .contains("checklist deploy"),
        "assembled_text doit contenir le body du skill seedé, got: {}",
        resp["assembled_text"]
    );
}
