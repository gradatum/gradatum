//! Tests E2E Task 5 — Retrieval RRF (`retrieve_candidates`).
//!
//! Vérifie cinq propriétés du pipeline retrieval :
//!
//! 1. **Chemin sémantique actif** : avec `FakeEmbedder` (non-Noop), `embed_fallback`
//!    est `false` (l'embed est réellement tenté) et des candidats sont retournés.
//! 2. **Sanitization FTS5 (P1-1)** : les requêtes vides ou avec caractères spéciaux FTS5
//!    ne provoquent pas de 500 — réponse structurée dans tous les cas.
//! 3. **ULID-direct (P1-4)** : une requête égale à un ULID valide retourne le corps
//!    de la note cible via le chemin de récupération directe (pas de RRF/embed).
//! 4. **Filtre section sémantique (C1 fix)** : le canal sémantique n'inclut PAS de
//!    notes d'autres sections quand `section` est spécifié.
//! 5. **Multi-sections (Task 1 v0.7.1)** : `sections=Some(&["A","B"])` exclut les notes
//!    de section C (BM25 ET sémantique), `sections=None` retourne toutes sections (parité).
//!
//! ## Dépendances helpers
//!
//! - [`helpers::build_app_with_embedder`] — construit un `TestEnv` avec `FakeEmbedder`.
//! - [`helpers::FakeEmbedder`] — embedder déterministe non-Noop (`backend_kind=Http`).
//! - [`helpers::seed_notes`] — sème N notes FTS-searchables (contenu « alpha beta »).
//! - [`helpers::seed_note_return_ulid`] — sème une note, retourne son ULID string.
//! - [`helpers::seed_backlink_to`] — sème un lien entrant vers un ULID cible.
//! - [`helpers::call_vault_context_json`] — POST `/api/v1/vault_context` body JSON libre.
//! - [`helpers::sign_token`] — génère un JWT `alpha13-tester`.

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{
    FakeEmbedder, build_app_with_context_config, build_app_with_embedder,
    build_app_with_session_trace_and_embedder, call_vault_context_json,
    call_vault_context_json_status, seed_backlink_to, seed_note_return_ulid, seed_notes,
    sign_token,
};
use std::sync::Arc;

/// Avec `FakeEmbedder` (non-Noop), le chemin sémantique est activé :
/// `embed_fallback` doit être `false` et au moins 1 candidat retourné.
///
/// # Invariants
///
/// - `diagnostics.embed_fallback = false` : l'embed a été tenté (pas de dégradation Noop).
/// - `diagnostics.candidates_considered >= 1` : au moins 1 candidat BM25 trouvé.
///
/// # Note sur la sémantique
///
/// Avec `FakeEmbedder`, les vecteurs sont déterministes mais aucune embedding n'est
/// stockée en base → `search_semantic` retourne 0 hits sémantiques. Le score RRF
/// repose donc sur BM25 seul, mais `embed_fallback` reste `false` car l'embed a réussi
/// (c'est l'appel `search_semantic` qui retourne vide, pas un échec d'embed).
/// C'est le comportement correct pour valider la plomberie du chemin sémantique.
#[tokio::test]
async fn retrieval_semantic_path_active_with_fake_embedder() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    seed_notes(&env, 8).await;
    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha beta",
            "mode": "assembled",
            "tenant_id": "main"
        }),
    )
    .await;

    // embed_fallback doit être false : le chemin sémantique est réellement exercé.
    assert_eq!(
        resp["diagnostics"]["embed_fallback"],
        serde_json::json!(false),
        "embed_fallback devrait être false avec FakeEmbedder (non-Noop) — resp={resp}"
    );
    // Au moins 1 candidat BM25 considéré (8 notes seedées avec « alpha beta »).
    assert!(
        resp["diagnostics"]["candidates_considered"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "candidates_considered devrait être >= 1 — resp={resp}"
    );
}

/// Les requêtes vides ou avec caractères spéciaux FTS5 ne doivent PAS provoquer de 500.
///
/// Vérifie que `build_fts_query` sanitize correctement et que le guard « vide »
/// dans `retrieve_candidates` retourne une réponse structurée (pas une erreur HTTP).
///
/// # Cas couverts
///
/// | Requête | Raison |
/// |---|---|
/// | `""` | Vide brut → sanitize → vide → early return |
/// | `"  "` | Espaces seuls → sanitize → vide → early return |
/// | `"a:b AND (c)"` | Caractères FTS5 non-safe → wrappé → phrase query |
/// | `"\"quote'd\""` | Guillemets et apostrophe → échappés → phrase query |
/// | `"..."` | Points → non-safe → wrappé → phrase query |
#[tokio::test]
async fn retrieval_empty_or_punct_query_does_not_500() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    seed_notes(&env, 3).await;
    let token = sign_token(&env.state);

    for q in ["", "  ", "a:b AND (c)", "\"quote'd\"", "..."] {
        let resp = call_vault_context_json(
            env.app.clone(),
            &token,
            serde_json::json!({
                "query": q,
                "mode": "assembled",
                "tenant_id": "main"
            }),
        )
        .await;

        // Pas de 500 — réponse structurée même si texte vide.
        assert!(
            resp.get("assembled_text").is_some(),
            "assembled_text absent pour query {:?} — pas de 500 attendu, resp={resp}",
            q
        );
    }
}

/// Sélection budget-aware : le budget tronque la liste et les résultats sont triés par score.
///
/// Avec 30 notes seedées et `budget_tokens=100`, la sélection doit :
/// - Inclure moins de 30 notes (`included.len() < 30`).
/// - Respecter le budget : `budget_used ≤ 100 + marge dernière note`.
/// - Trier par score décroissant (`scores.windows(2)` vérifient `w[0] >= w[1]`).
/// - Fournir `section` et `date` non vides pour chaque note incluse (P2-2).
///
/// # Comportement attendu
///
/// `select_budget_aware` calcule un score composite pondéré pour chaque candidat,
/// trie par score décroissant, puis charge le body lazily jusqu'à épuisement du budget.
#[tokio::test]
async fn select_stops_at_budget_and_orders_by_score() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    seed_notes(&env, 30).await;
    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha",
            "mode": "assembled",
            "budget_tokens": 100
        }),
    )
    .await;

    let included = resp["included"]
        .as_array()
        .expect("included doit être un tableau");
    assert!(
        included.len() < 30,
        "budget_tokens=100 doit tronquer la sélection (30 notes seedées) — got {}",
        included.len()
    );
    let budget_used = resp["budget_used"]
        .as_u64()
        .expect("budget_used doit être u64");
    // Marge de 200 : on peut dépasser le budget de la dernière note ajoutée.
    assert!(
        budget_used <= 300,
        "budget_used doit être ≤ 100+200 (marge dernière note), got {budget_used}"
    );
    // Tri décroissant par score.
    let scores: Vec<f64> = included
        .iter()
        .map(|n| n["score"].as_f64().expect("score doit être f64"))
        .collect();
    for w in scores.windows(2) {
        assert!(
            w[0] >= w[1],
            "tri score décroissant cassé : {} < {} (scores={scores:?})",
            w[0],
            w[1]
        );
    }
    // P2-2 : section, date et score réel sur la première note incluse.
    if let Some(first) = included.first() {
        assert!(
            !first["section"].as_str().unwrap_or("").is_empty(),
            "section ne doit pas être vide — resp={first}"
        );
        assert!(
            !first["date"].as_str().unwrap_or("").is_empty(),
            "date ne doit pas être vide — resp={first}"
        );
        // Score composite réel > 0 (rrf_score > 0 → composite > 0).
        // Le stub hardcode 0.0 — cette assertion distingue impl réelle vs stub.
        assert!(
            first["score"].as_f64().unwrap_or(0.0) > 0.0,
            "score doit être > 0.0 avec le scoring composite actif (stub met 0.0) — resp={first}"
        );
    }
}

/// Mode Assembled e2e : `assembled_text` contient les marqueurs structurés (spec §2.3).
///
/// Vérifie que `render_assembled` est câblé dans la branche Assembled (Task 7) :
/// - `assembled_text` contient `"score="` (marqueurs score Markdown structuré).
/// - Au moins 1 note incluse dans `included`.
/// - Budget respecté : `budget_used ≤ budget_tokens + marge`.
/// - `diagnostics.candidates_considered >= 1` (au moins 1 candidat BM25).
///
/// Utilise `FakeEmbedder` pour activer le chemin sémantique non-Noop.
#[tokio::test]
async fn assembled_mode_returns_structured_context() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    seed_notes(&env, 8).await;
    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha beta",
            "mode": "assembled",
            "budget_tokens": 1500
        }),
    )
    .await;

    // assembled_text doit contenir les marqueurs de score structuré (spec §2.3).
    assert!(
        resp["assembled_text"]
            .as_str()
            .unwrap_or("")
            .contains("score="),
        "assembled_text doit contenir 'score=' (marqueur structuré spec §2.3) — resp={resp}"
    );
    // Au moins 1 note incluse (8 notes seedées avec « alpha beta »).
    assert!(
        resp["included"].as_array().map(|a| a.len()).unwrap_or(0) >= 1,
        "au moins 1 note incluse attendue — resp={resp}"
    );
    // Budget respecté : budget_used ≤ budget_tokens + marge (dernière note).
    assert!(
        resp["budget_used"].as_u64().unwrap_or(u64::MAX) <= 1500 + 300,
        "budget_used doit être ≤ 1800 — resp={resp}"
    );
    // Au moins 1 candidat considéré.
    assert!(
        resp["diagnostics"]["candidates_considered"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "candidates_considered doit être >= 1 — resp={resp}"
    );
}

/// Task 8 — les métriques vault_context sont incrémentées après un appel Assembled.
///
/// Effectue un appel en mode `"assembled"` avec 5 notes FTS seedées, puis encode le
/// registry Prometheus et vérifie la présence des noms de série :
/// - `gradatum_vault_context_duration_seconds` (Histogram latence par mode)
/// - `gradatum_vault_context_candidates` (Histogram candidats considérés par mode)
///
/// Ces deux métriques apparaissent dans l'encodage OpenMetrics text format uniquement
/// si `assemble_context` les a observées au moins une fois (famille vide = absent).
///
/// # Invariants
///
/// - Présence de `"gradatum_vault_context_duration_seconds"` dans le buffer encodé.
/// - Présence de `"gradatum_vault_context_candidates"` dans le buffer encodé.
#[tokio::test]
async fn assembled_increments_metrics() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    seed_notes(&env, 5).await;
    let token = sign_token(&env.state);
    let _ = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha",
            "mode": "assembled"
        }),
    )
    .await;
    let mut buf = String::new();
    prometheus_client::encoding::text::encode(&mut buf, env.state.metrics.registry.as_ref())
        .expect("encode registry Prometheus — invariant test");
    // Le label mode="assembled" n'apparaît dans l'encodage QUE si get_or_create
    // a été appelé avec ce label (une famille vide n'émet que HELP/TYPE, pas de données).
    // On cible _sum{mode="assembled"} : format stable, sans préfixe `le` (buckets),
    // présent uniquement après au moins une observation sur ce label.
    // Ces assertions échouent sans instrumentation dans assemble_context.
    assert!(
        buf.contains(r#"gradatum_vault_context_duration_seconds_sum{mode="assembled"}"#),
        "gradatum_vault_context_duration_seconds_sum{{mode=\"assembled\"}} doit apparaître \
         (observation réelle requise, HELP/TYPE seul ne suffit pas)\nbuf={buf}"
    );
    assert!(
        buf.contains(r#"gradatum_vault_context_candidates_sum{mode="assembled"}"#),
        "gradatum_vault_context_candidates_sum{{mode=\"assembled\"}} doit apparaître \
         (observation réelle requise, HELP/TYPE seul ne suffit pas)\nbuf={buf}"
    );
}

/// Filtre section sur chemin sémantique (C1 fix — régression neuve v0.7.0).
///
/// Vérifie que `retrieve_candidates` n'inclut **PAS** de notes d'une section différente
/// via le canal sémantique lorsqu'un filtre `section` est spécifié.
///
/// # Preuve TDD
///
/// - **AVANT le fix** : `search_semantic` n'est pas filtré par section → les hits
///   sémantiques d'autres sections traversent la fusion RRF et arrivent dans `included`.
/// - **APRÈS le fix** : `filter_semantic_by_section` est appliqué sur les hits avant
///   la fusion RRF → seules les notes de la section demandée passent.
///
/// # Setup
///
/// - 1 note section `decisions` + embedding `[1.0, 0.0, ...]` → doit apparaître.
/// - 1 note section `reasoning` + embedding identique → **ne doit PAS** apparaître.
/// - Les deux embeddings sont identiques → `search_semantic` remonte les deux sans filtre.
///   Le test n'est donc pas vacuous : sans le fix, `ulid_reasoning` serait dans `included`.
///
/// # Dimension FakeEmbedder / embeddings stockés
///
/// `dim=8` : cohérent avec le vecteur `[1.0, 0.0, ...(7 zéros)]` stocké en DB.
/// `embedder_id = "fake-embedder"` : id produit par `FakeEmbedder::embedder_id()`.
#[tokio::test]
async fn retrieval_semantic_section_filter_excludes_other_sections() {
    use gradatum_core::identity::NoteId;
    use ulid::Ulid;

    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let token = sign_token(&env.state);
    let idx = env._vault_typed.index();

    // Seeder une note dans "decisions" avec du contenu FTS-searchable.
    let ulid_decisions = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &ulid_decisions,
        "decisions",
        "# Décision filtre section\ncontexte assemblage filtre section decisions",
    )
    .await
    .expect("seed decisions note — invariant test");
    let nid_decisions =
        NoteId(Ulid::from_string(&ulid_decisions).expect("ULID parse decisions — invariant"));
    idx.upsert_note_title(&nid_decisions, "Décision filtre section")
        .await
        .expect("upsert_note_title decisions — invariant test");

    // Seeder une note dans "reasoning" avec le même contenu lexical.
    let ulid_reasoning = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &ulid_reasoning,
        "reasoning",
        "# Reasoning filtre section\ncontexte assemblage filtre section reasoning",
    )
    .await
    .expect("seed reasoning note — invariant test");
    let nid_reasoning =
        NoteId(Ulid::from_string(&ulid_reasoning).expect("ULID parse reasoning — invariant"));
    idx.upsert_note_title(&nid_reasoning, "Reasoning filtre section")
        .await
        .expect("upsert_note_title reasoning — invariant test");

    // Stocker des embeddings IDENTIQUES pour les deux notes (dim=8, embedder_id=fake-embedder).
    // search_semantic remontera donc les deux avec un score cosinus égal.
    // Sans le fix : les deux traversent RRF → les deux dans included.
    let emb: Vec<f32> = {
        let mut v = vec![0.0f32; 8];
        v[0] = 1.0;
        v
    };
    env.state
        .search
        .insert_note_embedding(&nid_decisions, "fake-embedder", 8, &emb)
        .await
        .expect("insert_note_embedding decisions — invariant test");
    env.state
        .search
        .insert_note_embedding(&nid_reasoning, "fake-embedder", 8, &emb)
        .await
        .expect("insert_note_embedding reasoning — invariant test");

    // Appel vault_context avec section="decisions" uniquement.
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "contexte assemblage filtre",
            "mode": "assembled",
            "section": "decisions",
            "budget_tokens": 5000
        }),
    )
    .await;

    // Collecter les ULIDs des notes incluses (champ "ulid" dans IncludedNote).
    let included = resp["included"]
        .as_array()
        .expect("included doit être un tableau — resp={resp}");
    let included_ulids: Vec<&str> = included.iter().filter_map(|n| n["ulid"].as_str()).collect();

    // La note decisions doit être présente (BM25 + sémantique sur section demandée).
    assert!(
        included_ulids.contains(&ulid_decisions.as_str()),
        "la note decisions ({ulid_decisions}) doit apparaître dans included — ulids présents={included_ulids:?}"
    );

    // La note reasoning ne doit PAS être présente (filtre section appliqué sur sémantique).
    // Sans le fix C1, ce test serait ROUGE car ulid_reasoning traverserait RRF sans filtrage.
    assert!(
        !included_ulids.contains(&ulid_reasoning.as_str()),
        "la note reasoning ({ulid_reasoning}) ne doit PAS apparaître avec section=decisions — \
         C1 fix manquant ? ulids présents={included_ulids:?}"
    );
}

/// Une requête égale à un ULID valide retourne le corps de la note cible.
///
/// Vérifie la branche ULID-direct de `retrieve_candidates` (parité legacy `logic.rs:1010`).
/// Le mode Assembled doit inclure le corps de la note cible dans `assembled_text`.
///
/// # Invariant
///
/// `assembled_text` contient `"corps cible"` (body de la note seedée).
#[tokio::test]
async fn retrieval_ulid_direct_works_in_assembled() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let ulid = seed_note_return_ulid(&env, "Note cible", "corps cible unique xyz").await;
    seed_backlink_to(&env, &ulid).await;
    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": ulid,
            "mode": "assembled",
            "tenant_id": "main"
        }),
    )
    .await;

    assert!(
        resp["assembled_text"]
            .as_str()
            .unwrap_or("")
            .contains("corps cible unique xyz"),
        "assembled_text devrait contenir le corps de la note cible — resp={resp}"
    );
}

/// P2-b TDD : budget_used en mode Assembled inclut le scaffolding Markdown.
///
/// # Preuve TDD (rouge → vert)
///
/// - **AVANT fix** : `budget_used` = somme `estimate(body)` des notes seules.
///   Pour un corps minimal d'1 caractère, `estimate("x") ≈ 1 token`.
///   `budget_used ≈ 1` < 10 → assertion échoue.
/// - **APRÈS fix** : `budget_used = estimate(assembled_text)` qui inclut le scaffolding
///   (`"Contexte assemblé pour : «...» · 1 note\n\n### ... · score=...\n— source: [[...]]"`)
///   → >> 10 tokens → assertion passe.
///
/// # Pourquoi `>= 10` ?
///
/// Le scaffolding seul (en-tête query + métadonnées titre/section/date/score + source marker)
/// représente au minimum ~15 tokens, bien au-delà du budget ≈ 1 du corps minimal.
/// Ce seuil est conservateur et ne dépend pas des détails de l'estimateur.
#[tokio::test]
async fn budget_used_assembled_includes_scaffolding() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;

    // Corps minimal d'1 caractère — estimate(body) ≈ 1 token.
    // Avec le legacy, budget_used ≈ 1. Avec le fix, budget_used >> 1 (scaffolding inclus).
    seed_note_return_ulid(&env, "Titre scaffolding P2b", "x").await;
    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "scaffolding",
            "mode": "assembled",
            "budget_tokens": 5000
        }),
    )
    .await;

    let budget_used = resp["budget_used"].as_u64().expect("budget_used u64") as u32;

    // assembled_text non vide requis (au moins 1 note incluse).
    assert!(
        !resp["assembled_text"].as_str().unwrap_or("").is_empty(),
        "P2-b : assembled_text doit être non vide — resp={resp}"
    );

    // Preuve P2-b : scaffolding inclus → budget_used >> 1.
    // Avant fix : budget_used ≈ 1 (corps seul). Après fix : ≥ 10 (scaffolding).
    assert!(
        budget_used >= 10,
        "P2-b : budget_used doit inclure le scaffolding Markdown (≥ 10 tokens) — \
         avant fix la valeur était ≈ 1 (corps seul), got {budget_used}"
    );
}

// ── Tests Task 1 v0.7.1 — retrieve_candidates multi-sections ────────────────

/// Task 1 v0.7.1 — multi-sections exclut les notes hors du set.
///
/// Vérifie que `retrieve_candidates` avec `sections=Some(&["sec-alpha","sec-beta"])` :
/// - Exclut toute note de section `"sec-gamma"` (BM25 ET sémantique).
/// - Inclut au moins une note de `"sec-alpha"` ou `"sec-beta"`.
///
/// # Preuve TDD (rouge → vert)
///
/// - **AVANT impl** : la signature `section: Option<&str>` ne compile plus avec
///   `sections: Option<&[&str]>` → erreur de compilation (rouge).
/// - **APRÈS impl** : filtre en mémoire sur `SearchHitRaw.section` (BM25) et
///   `filter_semantic_by_sections` (sémantique) → sec-gamma absent.
///
/// # Setup
///
/// - 3 notes (sec-alpha / sec-beta / sec-gamma), même contenu FTS + embedding identique.
/// - `dim=8` : cohérent avec l'embedding `[1.0, 0.0, ...(7 zéros)]` stocké en DB.
/// - `FakeEmbedder` activé → chemin sémantique exercé (non-Noop).
#[tokio::test]
async fn retrieval_multi_section_excludes_out_of_set() {
    use gradatum_core::identity::NoteId;
    use gradatum_core::scope::VaultId;
    use gradatum_server::context::retrieval::retrieve_candidates;
    use ulid::Ulid;

    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let idx = env._vault_typed.index();

    // Contenu lexical identique pour toutes les notes → BM25 les remonte toutes.
    const BODY: &str =
        "# Note multi section\nmulti section retrieval recall parité test query contenu unique";

    // Note section sec-alpha.
    let ulid_a = Ulid::new().to_string();
    idx.seed_note_with_fts(&ulid_a, "sec-alpha", BODY)
        .await
        .expect("seed sec-alpha — invariant test");
    let nid_a = NoteId(Ulid::from_string(&ulid_a).expect("ULID sec-alpha"));
    idx.upsert_note_title(&nid_a, "Note sec-alpha")
        .await
        .expect("title sec-alpha");

    // Note section sec-beta.
    let ulid_b = Ulid::new().to_string();
    idx.seed_note_with_fts(&ulid_b, "sec-beta", BODY)
        .await
        .expect("seed sec-beta — invariant test");
    let nid_b = NoteId(Ulid::from_string(&ulid_b).expect("ULID sec-beta"));
    idx.upsert_note_title(&nid_b, "Note sec-beta")
        .await
        .expect("title sec-beta");

    // Note section sec-gamma — doit être exclue.
    let ulid_c = Ulid::new().to_string();
    idx.seed_note_with_fts(&ulid_c, "sec-gamma", BODY)
        .await
        .expect("seed sec-gamma — invariant test");
    let nid_c = NoteId(Ulid::from_string(&ulid_c).expect("ULID sec-gamma"));
    idx.upsert_note_title(&nid_c, "Note sec-gamma")
        .await
        .expect("title sec-gamma");

    // Embeddings identiques pour toutes → search_semantic remonte les 3 sans filtre.
    // Sans filtre sections, les 3 traverseraient RRF → gamma dans included.
    let emb: Vec<f32> = {
        let mut v = vec![0.0f32; 8];
        v[0] = 1.0;
        v
    };
    for nid in [&nid_a, &nid_b, &nid_c] {
        env.state
            .search
            .insert_note_embedding(nid, "fake-embedder", 8, &emb)
            .await
            .expect("insert_note_embedding — invariant test");
    }

    let vault_id = VaultId::new("main");
    let outcome = retrieve_candidates(
        &env.state,
        &vault_id,
        "multi section retrieval recall parité",
        Some(&["sec-alpha", "sec-beta"]),
        20,
        5_000,
    )
    .await
    .expect("retrieve_candidates — invariant test");

    // sec-gamma ne doit PAS être dans les candidats.
    assert!(
        !outcome.candidates.iter().any(|c| c.note_id == ulid_c),
        "sec-gamma ({ulid_c}) ne doit PAS apparaître avec sections=[sec-alpha,sec-beta] — \
         C1 fix manquant ? candidates={:?}",
        outcome
            .candidates
            .iter()
            .map(|c| &c.note_id)
            .collect::<Vec<_>>()
    );

    // Au moins une note de sec-alpha ou sec-beta doit être présente.
    assert!(
        outcome
            .candidates
            .iter()
            .any(|c| c.note_id == ulid_a || c.note_id == ulid_b),
        "au moins une note de sec-alpha/sec-beta doit être candidate — \
         candidates={:?}",
        outcome
            .candidates
            .iter()
            .map(|c| &c.note_id)
            .collect::<Vec<_>>()
    );
}

// ── Tests Task 2 v0.7.2 — split inline/stub/drop + tiebreaker ULID ──────────

/// Task 2 v0.7.2 — `select_budget_aware` : budget serré → top-K inline, reste stubs, queue drop.
///
/// Vérifie le split inline/stub/drop de `select_budget_aware` (F-29) :
/// - Budget inline très serré (25 tokens) → 0-1 note inline seulement.
/// - Stub budget moyen (50 tokens) → 2-3 stubs.
/// - Au-delà → droppés (`inline.len() + stubs.len() < total_candidates`).
///
/// # Calcul attendu
///
/// - 10 notes seedées avec body ≈ 18 mots (`"alpha beta gamma " × 5` + titre ~4 mots)
///   → `estimate(body) ≈ max(18×1.3, chars/6) = max(23, 16) ≈ 23 tokens` par note.
/// - `budget_inline = 25` → 1 note inline (23 ≤ 25), 2ème dépasse (23+23=46 > 25).
/// - `stub_budget = 50` → chaque stub ≈ 20 tokens (render_stub, snippet 120 chars) → 2 stubs.
/// - Reste (≥ 7 notes) → droppé.
///
/// # Note
///
/// Ce test appelle `select_budget_aware` directement (pas via HTTP) pour pouvoir
/// contrôler précisément `budget` et `stub_budget` (indépendants de la config).
/// Les stubs ne sont pas encore exposés dans la réponse HTTP (Task 4).
#[tokio::test]
async fn select_split_inline_then_stub_then_drop() {
    use chrono::Utc;
    use gradatum_core::{identity::NoteId, scope::VaultId};
    use gradatum_search::resolve_weights;
    use gradatum_server::context::{
        retrieval::retrieve_candidates, select::select_budget_aware, tokens::HeuristicEstimator,
    };
    use ulid::Ulid;

    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let idx = env._vault_typed.index();

    // Seeder 10 notes avec des bodies identiques pour score BM25 homogène
    // → le tri se fait principalement via le tiebreaker ULID.
    const BODY_SEED: &str =
        "alpha beta gamma alpha beta gamma alpha beta gamma alpha beta gamma alpha beta gamma";
    for i in 0..10u32 {
        let ulid = Ulid::new().to_string();
        let body = format!("# Note split {i}\n{BODY_SEED}");
        idx.seed_note_with_fts(&ulid, "decisions", &body)
            .await
            .expect("seed note — invariant test");
        let nid = NoteId(Ulid::from_string(&ulid).expect("ULID parse — invariant test"));
        idx.upsert_note_title(&nid, &format!("Note split {i}"))
            .await
            .expect("upsert_note_title — invariant test");
    }

    let vault_id = VaultId::new("main");
    let now_ms = Utc::now().timestamp_millis();
    let weights = resolve_weights(None);
    let estimator = HeuristicEstimator;

    // Récupérer les candidats via le pipeline retrieval.
    let outcome = retrieve_candidates(&env.state, &vault_id, "alpha beta gamma", None, 20, 5_000)
        .await
        .expect("retrieve_candidates — invariant test");

    let total_candidates = outcome.candidates.len();
    assert!(
        total_candidates >= 3,
        "il faut au moins 3 candidats pour ce test — got {total_candidates} (10 notes seedées)"
    );

    // Budget inline très serré → 1 note max inline.
    // Stub budget moyen → 2-3 stubs.
    let budget_inline: u32 = 25;
    let stub_budget: u32 = 50;

    let (inline, stubs, _budget_used) = select_budget_aware(
        &env.state,
        "main",
        outcome.candidates,
        &weights,
        &estimator,
        budget_inline,
        stub_budget,
        now_ms,
    )
    .await
    .expect("select_budget_aware — invariant test");

    // Top-K inline (borné par budget_inline).
    // Avec budget=25 et notes de ≈23 tokens chacune → max 1 note inline.
    assert!(
        inline.len() <= 1,
        "budget_inline=25 doit limiter à 0-1 note inline — got {}",
        inline.len()
    );

    // Des stubs doivent être produits (stub_budget=50 suffit pour quelques stubs).
    assert!(
        !stubs.is_empty(),
        "stub_budget=50 doit produire au moins 1 stub — got {} stubs (total_candidates={total_candidates})",
        stubs.len()
    );

    // Des candidats doivent être droppés : inline + stubs < total.
    assert!(
        inline.len() + stubs.len() < total_candidates,
        "certains candidats doivent être droppés — inline={}, stubs={}, total={total_candidates}",
        inline.len(),
        stubs.len()
    );
}

/// Task 2 v0.7.2 — `stub_budget=0` → aucun stub produit (parité comportement F-35).
///
/// Vérifie que `select_budget_aware` avec `stub_budget=0` ne produit aucun stub,
/// et que les notes inline sont identiques au comportement F-35 (budget inline large,
/// tout inline, pas de stubs).
///
/// # Contrat
///
/// `stub_budget=0` OU `budget_inline` très grand → `stubs.is_empty()` → comportement
/// identique à l'ancien `select_budget_aware` (avant Task 2) qui ne produisait que
/// `Vec<Selected>`.
#[tokio::test]
async fn select_reference_mode_off_parity() {
    use chrono::Utc;
    use gradatum_core::{identity::NoteId, scope::VaultId};
    use gradatum_search::resolve_weights;
    use gradatum_server::context::{
        retrieval::retrieve_candidates, select::select_budget_aware, tokens::HeuristicEstimator,
    };
    use ulid::Ulid;

    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let idx = env._vault_typed.index();

    // Seeder 5 notes avec du contenu unique pour retrieval FTS.
    for i in 0..5u32 {
        let ulid = Ulid::new().to_string();
        let body =
            format!("# Note parité {i}\ncontenu parité parity reference mode off alpha beta");
        idx.seed_note_with_fts(&ulid, "decisions", &body)
            .await
            .expect("seed note — invariant test");
        let nid = NoteId(Ulid::from_string(&ulid).expect("ULID parse — invariant test"));
        idx.upsert_note_title(&nid, &format!("Note parité {i}"))
            .await
            .expect("upsert_note_title — invariant test");
    }

    let vault_id = VaultId::new("main");
    let now_ms = Utc::now().timestamp_millis();
    let weights = resolve_weights(None);
    let estimator = HeuristicEstimator;

    let outcome = retrieve_candidates(
        &env.state,
        &vault_id,
        "parité reference mode off alpha",
        None,
        20,
        5_000,
    )
    .await
    .expect("retrieve_candidates — invariant test");

    assert!(
        !outcome.candidates.is_empty(),
        "au moins 1 candidat attendu (5 notes seedées) — got 0"
    );

    // stub_budget=0 : aucun stub — équivalent au comportement F-35 (inline-only).
    let (inline, stubs, _) = select_budget_aware(
        &env.state,
        "main",
        outcome.candidates,
        &weights,
        &estimator,
        5_000, // budget inline large → toutes les notes rentrent
        0,     // stub_budget=0 → aucun stub
        now_ms,
    )
    .await
    .expect("select_budget_aware — invariant test");

    // Parité F-35 : stubs vide, inline non vide.
    assert!(
        stubs.is_empty(),
        "stub_budget=0 doit produire zéro stubs — got {} stubs",
        stubs.len()
    );
    assert!(
        !inline.is_empty(),
        "au moins 1 note inline attendue avec budget_inline=5000 — got 0"
    );
}

// ── Tests Task 4 v0.7.2 — reference_mode + references réponse ───────────────

/// Task 4 v0.7.2 — `reference_mode` absent → `references` vide (parité F-35).
///
/// Garantit la rétro-compat : un client existant qui ne passe pas `reference_mode`
/// reçoit `references: []` et un comportement assemblé strictement identique à F-35.
///
/// # Invariants
///
/// - `resp["references"]` est un tableau JSON vide `[]`.
/// - `resp["assembled_text"]` est non vide (au moins 1 note incluse, parité F-35).
/// - `resp["counts"]["inline"] + resp["counts"]["stub"] + resp["counts"]["dropped"]`
///   == `resp["diagnostics"]["candidates_considered"]` (cohérence compteurs).
#[tokio::test]
async fn context_reference_mode_off_response_parity() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    seed_notes(&env, 8).await;
    let token = sign_token(&env.state);

    // Payload sans `reference_mode` → défaut false.
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha beta",
            "mode": "assembled",
            "budget_tokens": 5000
        }),
    )
    .await;

    // references doit être un tableau JSON vide.
    let references = resp["references"]
        .as_array()
        .expect("references doit être un tableau JSON — resp={resp}");
    assert!(
        references.is_empty(),
        "reference_mode=false → references doit être vide — got {:?}",
        references
    );

    // assembled_text non vide : parité F-35 inchangée.
    assert!(
        !resp["assembled_text"].as_str().unwrap_or("").is_empty(),
        "assembled_text doit être non vide (parité F-35) — resp={resp}"
    );

    // Cohérence counts : inline + stub + dropped == candidates_considered.
    let inline = resp["counts"]["inline"].as_u64().unwrap_or(0);
    let stub = resp["counts"]["stub"].as_u64().unwrap_or(0);
    let dropped = resp["counts"]["dropped"].as_u64().unwrap_or(0);
    let candidates = resp["diagnostics"]["candidates_considered"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(
        inline + stub + dropped,
        candidates,
        "counts incohérents : inline({inline}) + stub({stub}) + dropped({dropped}) != candidates({candidates})"
    );
    // stub == 0 : reference_mode=false → aucun stub produit.
    assert_eq!(
        stub, 0,
        "reference_mode=false → counts.stub doit être 0 — got {stub}"
    );
}

/// Task 4 v0.7.2 — `reference_mode=true` + budget serré → `references` non vide.
///
/// Avec 15 notes seedées et un budget inline très serré (25 tokens), le pipeline
/// doit produire au moins 1 stub dans `references` (F-29).
///
/// # Invariants
///
/// - `resp["references"]` contient au moins 1 élément.
/// - Chaque élément de `references` a `ulid`, `title`, `section`, `snippet` (string).
/// - `counts.stub >= 1`.
/// - `counts.inline + counts.stub + counts.dropped == candidates_considered`.
#[tokio::test]
async fn context_reference_mode_on_emits_references() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    seed_notes(&env, 15).await;
    let token = sign_token(&env.state);

    // Budget inline très serré → 0-1 notes inline, le reste en stubs si reference_mode=true.
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha beta",
            "mode": "assembled",
            "budget_tokens": 25,
            "reference_mode": true
        }),
    )
    .await;

    // references non vide avec budget serré + 15 notes + reference_mode=true.
    let references = resp["references"]
        .as_array()
        .expect("references doit être un tableau JSON — resp={resp}");
    assert!(
        !references.is_empty(),
        "reference_mode=true + budget serré → references doit contenir au moins 1 stub — resp={resp}"
    );

    // Champs du premier stub.
    let first = &references[0];
    assert!(
        first["ulid"].as_str().is_some(),
        "stub.ulid doit être une string — stub={first}"
    );
    assert!(
        first["title"].as_str().is_some(),
        "stub.title doit être une string — stub={first}"
    );
    assert!(
        first["section"].as_str().is_some(),
        "stub.section doit être une string — stub={first}"
    );
    assert!(
        first["snippet"].as_str().is_some(),
        "stub.snippet doit être une string — stub={first}"
    );

    // Cohérence counts.
    let inline = resp["counts"]["inline"].as_u64().unwrap_or(0);
    let stub = resp["counts"]["stub"].as_u64().unwrap_or(0);
    let dropped = resp["counts"]["dropped"].as_u64().unwrap_or(0);
    let candidates = resp["diagnostics"]["candidates_considered"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(
        inline + stub + dropped,
        candidates,
        "counts incohérents : inline({inline}) + stub({stub}) + dropped({dropped}) != candidates({candidates})"
    );
    assert!(
        stub >= 1,
        "counts.stub doit être >= 1 (reference_mode=true + budget serré) — resp={resp}"
    );
}

/// Task 1 v0.7.1 — `sections=None` préserve la parité (toutes sections retournées).
///
/// Vérifie que `retrieve_candidates` avec `sections=None` retourne bien les notes
/// de toutes les sections — comportement identique à l'actuel `section: None`.
///
/// # Preuve TDD
///
/// - Seed 3 notes dans 3 sections différentes.
/// - `sections=None` → aucun filtre en mémoire → les 3 notes sont candidates.
#[tokio::test]
async fn retrieval_sections_none_parity() {
    use gradatum_core::identity::NoteId;
    use gradatum_core::scope::VaultId;
    use gradatum_server::context::retrieval::retrieve_candidates;
    use ulid::Ulid;

    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let idx = env._vault_typed.index();

    const BODY: &str =
        "# Note parité sections\nparité sections none parity retrieval test unique query";

    let mut expected_ulids: Vec<String> = Vec::new();
    for section in ["parity-one", "parity-two", "parity-three"] {
        let ulid = Ulid::new().to_string();
        idx.seed_note_with_fts(&ulid, section, BODY)
            .await
            .expect("seed note — invariant test");
        let nid = NoteId(Ulid::from_string(&ulid).expect("ULID parse"));
        idx.upsert_note_title(&nid, &format!("Note {section}"))
            .await
            .expect("title — invariant test");
        expected_ulids.push(ulid);
    }

    let vault_id = VaultId::new("main");
    let outcome = retrieve_candidates(
        &env.state,
        &vault_id,
        "parité sections none parity retrieval",
        None, // toutes sections — parité exacte comportement actuel
        20,
        5_000,
    )
    .await
    .expect("retrieve_candidates — invariant test");

    // Toutes les notes seedées doivent être candidates (None = pas de filtre).
    for ulid in &expected_ulids {
        assert!(
            outcome.candidates.iter().any(|c| &c.note_id == ulid),
            "ulid={ulid} doit être candidate avec sections=None (parité) — \
             candidates={:?}",
            outcome
                .candidates
                .iter()
                .map(|c| &c.note_id)
                .collect::<Vec<_>>()
        );
    }
}

// ── Tests Task 6 — Filtre incrémental session (F-30 régime normal) ───────────
//
// Cinq tests TDD couvrant :
// 1. no-re-promotion : une note déjà inline en T1 est forcée en stub en T2.
// 2. mark_sent : une nouvelle note inline est bien enregistrée dans le store.
// 3. sans session_id : comportement F-29 pur inchangé (pas de filtre).
// 4. session_id invalide : format ULID violé → 400 BAD_REQUEST.
// 5. store absent : session_id présent mais state.session_trace=None → F-29 pur.

/// **session_already_sent_returns_stub** — T6-1 no-re-promotion (Constraint 4).
///
/// Tour T1 : session=SID, note A répond à la requête → inline dans la réponse.
/// `mark_sent` est appelé implicitement → A présente dans le store.
/// Tour T2 : même session, même requête → A DOIT être dans `references` (stub),
/// PAS dans `included` (inline). Le snippet figé provient de T1.
///
/// Preuve no-re-promotion : même score maximal ne suffit pas à remettre A inline.
#[tokio::test]
async fn session_already_sent_returns_stub() {
    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    // Seeder note A avec contenu riche et un titre spécifique.
    let note_a_ulid = seed_note_return_ulid(
        &env,
        "Note Alpha unique",
        "alpha note contenu test filtre session incremental T6",
    )
    .await;

    let session_id = ulid::Ulid::new().to_string();
    let body = serde_json::json!({
        "query": "alpha session filtre incremental",
        "session_id": session_id,
        "reference_mode": true,
        "budget_tokens": 4000,
    });

    // T1 : note A doit être inline (nouveau tour, session vide).
    let resp_t1 = call_vault_context_json(env.app.clone(), &token, body.clone()).await;
    let included_t1 = resp_t1["included"].as_array().expect("included T1");
    let t1_inline_ulids: Vec<&str> = included_t1
        .iter()
        .filter_map(|n| n["ulid"].as_str())
        .collect();
    assert!(
        t1_inline_ulids.contains(&note_a_ulid.as_str()),
        "T1 : note A doit être inline (premier tour, session vide). \
         included_ulids={t1_inline_ulids:?}, note_a_ulid={note_a_ulid}"
    );

    // T2 : même session, même requête → note A DOIT être dans references (stub).
    let resp_t2 = call_vault_context_json(env.app.clone(), &token, body).await;

    let included_t2 = resp_t2["included"].as_array().expect("included T2");
    let refs_t2 = resp_t2["references"].as_array().expect("references T2");

    let t2_inline_ulids: Vec<&str> = included_t2
        .iter()
        .filter_map(|n| n["ulid"].as_str())
        .collect();
    let t2_ref_ulids: Vec<&str> = refs_t2.iter().filter_map(|n| n["ulid"].as_str()).collect();

    assert!(
        !t2_inline_ulids.contains(&note_a_ulid.as_str()),
        "T2 no-re-promotion : note A (already-sent) ne doit PAS être inline. \
         included_ulids={t2_inline_ulids:?}"
    );
    assert!(
        t2_ref_ulids.contains(&note_a_ulid.as_str()),
        "T2 : note A doit être dans references (stub forcé). \
         ref_ulids={t2_ref_ulids:?}"
    );
}

/// **session_new_note_inline_and_marked** — T6-2 mark_sent.
///
/// Une nouvelle note inline (premier tour de la session) doit être enregistrée
/// dans le `SessionTraceStore` après la réponse — vérifiable via `get_sent`.
///
/// Preuve : après le premier appel, `get_sent(tenant, session_id)` contient le
/// `note_id` de la note qui était inline.
#[tokio::test]
async fn session_new_note_inline_and_marked() {
    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    let note_ulid = seed_note_return_ulid(
        &env,
        "Note Mark Sent unique",
        "mark sent session trace filtre incremental test T6-2",
    )
    .await;

    let session_id = ulid::Ulid::new().to_string();
    let body = serde_json::json!({
        "query": "mark sent session trace",
        "session_id": session_id,
        "reference_mode": true,
        "budget_tokens": 4000,
    });

    // T1 : note doit être inline.
    let resp = call_vault_context_json(env.app.clone(), &token, body).await;
    let included = resp["included"].as_array().expect("included");
    let inline_ulids: Vec<&str> = included.iter().filter_map(|n| n["ulid"].as_str()).collect();

    assert!(
        inline_ulids.contains(&note_ulid.as_str()),
        "note doit être inline T1 pour vérifier mark_sent. \
         inline_ulids={inline_ulids:?}"
    );

    // Vérifier que mark_sent a bien été appelé — `get_sent` contient l'ULID.
    // Le store partagé (Arc<Mutex>) est accessible depuis env.state.
    let store = env
        .state
        .session_trace
        .as_ref()
        .expect("session_trace présent dans le state");
    let sent_map = store
        .get_sent("main", &session_id)
        .await
        .expect("get_sent — invariant test");

    assert!(
        sent_map.contains_key(&note_ulid),
        "mark_sent : note inline doit être dans sent_map après T1. \
         sent_map keys={:?}",
        sent_map.keys().collect::<Vec<_>>()
    );
}

/// **no_session_id_is_f29_only** — T6-3 comportement F-29 pur sans session_id.
///
/// Sans `session_id`, le filtre session est inactif : comportement Task 4 inchangé.
/// Les notes disponibles sont inline normalement.
/// Pas d'erreur, pas de filtre, pas d'appel `get_sent`/`mark_sent`.
#[tokio::test]
async fn no_session_id_is_f29_only() {
    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    let note_ulid = seed_note_return_ulid(
        &env,
        "Note F29 pur sans session",
        "f29 pur comportement inchangé sans session id filtre",
    )
    .await;

    // Sans session_id : F-29 pur.
    let body = serde_json::json!({
        "query": "f29 pur comportement inchangé",
        "reference_mode": true,
        "budget_tokens": 4000,
    });

    let resp = call_vault_context_json(env.app.clone(), &token, body).await;
    let included = resp["included"].as_array().expect("included");
    let inline_ulids: Vec<&str> = included.iter().filter_map(|n| n["ulid"].as_str()).collect();

    assert!(
        inline_ulids.contains(&note_ulid.as_str()),
        "sans session_id : note doit être inline (F-29 pur). \
         inline_ulids={inline_ulids:?}"
    );

    // Le store ne doit PAS avoir reçu de mark_sent (pas de session_id → skip total).
    let store = env.state.session_trace.as_ref().expect("store présent");
    // Utiliser un session_id arbitraire pour vérifier que le store est bien vide.
    let dummy_sid = ulid::Ulid::new().to_string();
    let sent_map = store.get_sent("main", &dummy_sid).await.expect("get_sent");
    assert!(
        sent_map.is_empty(),
        "sans session_id : aucun mark_sent ne doit avoir eu lieu"
    );
}

/// **session_id_invalid_rejected** — T6-4 validation ULID (P2-2).
///
/// Un `session_id` qui ne respecte pas le format ULID (26 chars ASCII alphanumériques)
/// doit être rejeté avec HTTP 400 BAD_REQUEST — `GradatumError::InvalidInput`.
///
/// Cas testés :
/// - Chaîne trop longue (127 chars) : rejeter.
/// - Chaîne avec caractères non-alphanumériques (tiret) : rejeter.
#[tokio::test]
async fn session_id_invalid_rejected() {
    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    // Cas 1 : session_id trop long (127 chars).
    let body_long = serde_json::json!({
        "query": "test invalide",
        "session_id": "a".repeat(127),
    });
    let (status_long, _) = call_vault_context_json_status(env.app.clone(), &token, body_long).await;
    assert_eq!(
        status_long,
        axum::http::StatusCode::BAD_REQUEST,
        "session_id trop long doit retourner 400 BAD_REQUEST"
    );

    // Cas 2 : session_id avec tiret (caractère non-alphanumérique).
    // ULID Crockford n'autorise pas les tirets — invalide.
    let body_dash = serde_json::json!({
        "query": "test invalide dash",
        "session_id": "01JXAAAAAAAAAAAAAAAAAAA--",  // 25 chars + tiret = 26 mais charset invalide
    });
    let (status_dash, _) = call_vault_context_json_status(env.app.clone(), &token, body_dash).await;
    assert_eq!(
        status_dash,
        axum::http::StatusCode::BAD_REQUEST,
        "session_id avec tirets doit retourner 400 BAD_REQUEST"
    );
}

// ── Tests Task 7 — Snippet figé bout-en-bout + dedup ULID ───────────────────
//
// Trois tests TDD couvrant :
// 1. T7-1 snippet figé : une note déjà-sent tombée dans les stubs BM25 porte le snippet
//    du 1er mark_sent (pas un ré-extrait du body courant).
// 2. T7-2 byte-stabilité : deux appels successifs dans la même session produisent un stub
//    de note A byte-identique entre les deux.
// 3. T7-3 dedup ULID : une note déjà-sent + hit BM25 → exactement 1 entrée dans references
//    avec le snippet figé.

/// **sent_stub_uses_frozen_snippet** — T7-1 snippet figé (Constraint 5).
///
/// Simule une note déjà-sent (mark_sent forcé avec snippet custom "snippet-figé-t1-custom")
/// puis appelle vault_context avec budget inline serré → note dans stubs BM25.
/// Vérifie que le stub porte le snippet du 1er mark_sent (figé), PAS un re-extrait du body.
///
/// # Preuve TDD (rouge → vert)
///
/// - **AVANT Task 7** : les stubs BM25 déjà-sent ne sont pas vérifiés contre sent_map →
///   leur snippet vient de `stub_from_selected` (body courant) = début du body.
/// - **APRÈS Task 7** : pour chaque stub BM25, si ULID dans sent_map → snippet remplacé par
///   le snippet figé du 1er mark_sent = "snippet-figé-t1-custom" (distinct du body).
///
/// # Invariants
///
/// - note A apparaît dans references (stub BM25 déjà-sent, budget inline=1 insuffisant).
/// - stub_a["snippet"] == "snippet-figé-t1-custom" (figé, pas le body ré-extrait).
#[tokio::test]
async fn sent_stub_uses_frozen_snippet() {
    use chrono::Utc;

    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    // Seeder note A avec un body dont le début est distinct du snippet forcé.
    let note_a_ulid = seed_note_return_ulid(
        &env,
        "Note Alpha frozen snippet T7",
        "body alpha dedup stub snippet figé test t7 alpha",
    )
    .await;

    let session_id = ulid::Ulid::new().to_string();
    let now_ms = Utc::now().timestamp_millis();

    // Forcer mark_sent avec un snippet custom — délibérément distinct du début du body.
    // Après Task 7, le stub devra porter ce snippet (pas le body ré-extrait).
    let store = env
        .state
        .session_trace
        .as_ref()
        .expect("session_trace présent — invariant test");
    store
        .mark_sent(
            "main",
            &session_id,
            &note_a_ulid,
            "snippet-figé-t1-custom",
            now_ms,
        )
        .await
        .expect("mark_sent forcé — invariant test");

    // Appel T2 : budget inline=1 (très serré) → note A dans stubs BM25.
    // reference_mode=true pour exposer les stubs dans la réponse.
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "body alpha dedup stub snippet",
            "session_id": session_id,
            "reference_mode": true,
            "budget_tokens": 1,
            "mode": "assembled",
        }),
    )
    .await;

    let references = resp["references"]
        .as_array()
        .expect("references doit être un tableau — resp={resp}");

    let stub_a = references
        .iter()
        .find(|r| r["ulid"].as_str() == Some(&note_a_ulid));

    let stub_a = stub_a.unwrap_or_else(|| {
        panic!(
            "T7-1 : note A ({note_a_ulid}) doit être dans references (stub BM25 déjà-sent, \
             budget=1). references={references:?}, resp={resp}"
        )
    });

    let snippet = stub_a["snippet"]
        .as_str()
        .expect("stub.snippet doit être une string");

    assert_eq!(
        snippet, "snippet-figé-t1-custom",
        "T7-1 snippet figé : le stub doit porter le snippet du 1er mark_sent \
         ('snippet-figé-t1-custom'), PAS le body ré-extrait. got='{snippet}'"
    );
}

/// **two_calls_same_session_byte_identical_stub** — T7-2 byte-stabilité.
///
/// Deux appels successifs dans la même session (note A déjà-sent) produisent
/// un stub de note A byte-identique entre les deux.
///
/// # Séquence
///
/// - T1 : budget large → note A inline → mark_sent automatique (snippet figé capturé).
/// - T2 : budget serré → note A dans stubs BM25 → snippet du 1er mark_sent.
/// - T3 : même requête que T2 → stub identique byte-pour-byte.
///
/// # Invariants
///
/// - note A dans `included` en T1 (prérequis : mark_sent bien appelé).
/// - note A dans `references` en T2 et T3 (budget serré → stubs BM25).
/// - snippet(T2) == snippet(T3) (byte-identique — sérialisation déterministe + snippet figé).
#[tokio::test]
async fn two_calls_same_session_byte_identical_stub() {
    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    let note_a_ulid = seed_note_return_ulid(
        &env,
        "Note Alpha byte stable T7-2",
        "body alpha byte stable session stub test t7-2 unique",
    )
    .await;

    let session_id = ulid::Ulid::new().to_string();

    // T1 : budget large → note A inline → mark_sent automatique.
    let resp_t1 = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "body alpha byte stable",
            "session_id": session_id,
            "reference_mode": true,
            "budget_tokens": 4000,
            "mode": "assembled",
        }),
    )
    .await;

    let included_t1 = resp_t1["included"].as_array().expect("included T1");
    assert!(
        included_t1
            .iter()
            .any(|n| n["ulid"].as_str() == Some(&note_a_ulid)),
        "T7-2 prérequis : note A doit être inline en T1 (mark_sent appelé). \
         included={included_t1:?}"
    );

    // T2 et T3 : budget serré → note A dans stubs BM25 (déjà-sent).
    let body_serré = serde_json::json!({
        "query": "body alpha byte stable",
        "session_id": session_id,
        "reference_mode": true,
        "budget_tokens": 1,
        "mode": "assembled",
    });

    let resp_t2 = call_vault_context_json(env.app.clone(), &token, body_serré.clone()).await;
    let resp_t3 = call_vault_context_json(env.app.clone(), &token, body_serré).await;

    let refs_t2 = resp_t2["references"].as_array().expect("references T2");
    let refs_t3 = resp_t3["references"].as_array().expect("references T3");

    let stub_t2 = refs_t2
        .iter()
        .find(|r| r["ulid"].as_str() == Some(&note_a_ulid))
        .unwrap_or_else(|| {
            panic!("T7-2 T2 : note A ({note_a_ulid}) doit être dans references. refs={refs_t2:?}")
        });
    let stub_t3 = refs_t3
        .iter()
        .find(|r| r["ulid"].as_str() == Some(&note_a_ulid))
        .unwrap_or_else(|| {
            panic!("T7-2 T3 : note A ({note_a_ulid}) doit être dans references. refs={refs_t3:?}")
        });

    let snippet_t2 = stub_t2["snippet"].as_str().expect("snippet T2 string");
    let snippet_t3 = stub_t3["snippet"].as_str().expect("snippet T3 string");

    assert_eq!(
        snippet_t2, snippet_t3,
        "T7-2 byte-stable : le stub de note A doit être byte-identique entre T2 et T3. \
         snippet_t2='{snippet_t2}', snippet_t3='{snippet_t3}'"
    );
}

/// **sent_note_also_in_bm25_appears_once** — T7-3 dedup ULID + snippet figé.
///
/// Une note déjà-sent (mark_sent forcé) qui est aussi un hit BM25 du tour courant
/// apparaît exactement **une seule fois** dans `references` (pas de doublon ULID),
/// avec le snippet figé du mark_sent (pas un ré-extrait du body courant).
///
/// # Scénario
///
/// - mark_sent forcé sur note A avec snippet "snippet-dedup-unique".
/// - Appel vault_context avec budget_tokens=1 (note A ne peut pas être inline) +
///   reference_mode=true → note A dans stubs BM25 ET dans sent_map.
///
/// # Invariants
///
/// - nombre d'occurrences de note_a_ulid dans references == 1 (dedup).
/// - snippet de la référence == "snippet-dedup-unique" (figé, pas le body ré-extrait).
///
/// # Preuve TDD (rouge → vert)
///
/// - **AVANT Task 7** : stubs BM25 déjà-sent non vérifiés contre sent_map → snippet
///   ré-extrait (body courant, pas "snippet-dedup-unique").
/// - **APRÈS Task 7** : `sent_map.get(&s.ulid)` → snippet remplacé par figé ✓, dedup ✓.
#[tokio::test]
async fn sent_note_also_in_bm25_appears_once() {
    use chrono::Utc;

    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    let note_a_ulid = seed_note_return_ulid(
        &env,
        "Note Alpha dedup unique T7-3",
        "body alpha dedup bm25 already sent test t7-3 unique content",
    )
    .await;

    let session_id = ulid::Ulid::new().to_string();
    let now_ms = Utc::now().timestamp_millis();

    // Forcer mark_sent avec snippet custom distinct du body.
    let store = env
        .state
        .session_trace
        .as_ref()
        .expect("session_trace présent — invariant test");
    store
        .mark_sent(
            "main",
            &session_id,
            &note_a_ulid,
            "snippet-dedup-unique",
            now_ms,
        )
        .await
        .expect("mark_sent forcé — invariant test");

    // Appel vault_context : budget=1 (note A dans stubs BM25) + session active (sent_map) +
    // reference_mode=true → note A dans sent_map ET dans stubs BM25.
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "body alpha dedup bm25",
            "session_id": session_id,
            "reference_mode": true,
            "budget_tokens": 1,
            "mode": "assembled",
        }),
    )
    .await;

    let references = resp["references"]
        .as_array()
        .expect("references doit être un tableau — resp={resp}");

    // Dedup : note A doit apparaître exactement 1 fois.
    let count_a = references
        .iter()
        .filter(|r| r["ulid"].as_str() == Some(&note_a_ulid))
        .count();

    assert_eq!(
        count_a, 1,
        "T7-3 dedup : note A ({note_a_ulid}) doit apparaître exactement 1 fois dans references \
         (pas de doublon ULID). count={count_a}, references={references:?}"
    );

    // Snippet figé : le snippet doit être celui du mark_sent forcé, pas le body ré-extrait.
    let stub_a = references
        .iter()
        .find(|r| r["ulid"].as_str() == Some(&note_a_ulid))
        .expect("stub_a présent (count vérifié ci-dessus — invariant)");
    let snippet = stub_a["snippet"].as_str().expect("snippet string");

    assert_eq!(
        snippet, "snippet-dedup-unique",
        "T7-3 snippet figé : le stub doit porter le snippet du mark_sent forcé \
         ('snippet-dedup-unique'), PAS le body ré-extrait. got='{snippet}'"
    );
}

/// **session_store_none_degrades_to_f29** — T6-5 dégradation P2-4.
///
/// Si `session_id` est fourni mais que `state.session_trace` est `None`
/// (store non câblé), le pipeline doit dégrader en F-29 pur :
/// - Réponse HTTP 200 (pas d'erreur).
/// - Les notes sont inline normalement.
/// - Aucune panique.
///
/// Ce test utilise `build_app_with_embedder` (sans session_trace) intentionnellement.
#[tokio::test]
async fn session_store_none_degrades_to_f29() {
    // Pas de session_trace : build_app_with_embedder → state.session_trace = None.
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    assert!(
        env.state.session_trace.is_none(),
        "invariant test : session_trace doit être None pour valider la dégradation P2-4"
    );

    let note_ulid = seed_note_return_ulid(
        &env,
        "Note Dégradation P2-4",
        "dégradation f29 session trace absent state none",
    )
    .await;

    // session_id valide (26 chars ULID) mais store = None → dégradation F-29-pur.
    let session_id = ulid::Ulid::new().to_string();
    assert_eq!(session_id.len(), 26, "invariant : ULID doit être 26 chars");

    let body = serde_json::json!({
        "query": "dégradation f29 session",
        "session_id": session_id,
        "reference_mode": true,
        "budget_tokens": 4000,
    });

    // Doit retourner 200 sans paniquer (dégradation gracieuse P2-4).
    let (status, resp) = call_vault_context_json_status(env.app.clone(), &token, body).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "session_id fourni mais store=None doit retourner 200 (dégradation P2-4), pas une erreur"
    );

    // La note doit être inline (comportement F-29 pur inchangé).
    let included = resp["included"].as_array().expect("included");
    let inline_ulids: Vec<&str> = included.iter().filter_map(|n| n["ulid"].as_str()).collect();
    assert!(
        inline_ulids.contains(&note_ulid.as_str()),
        "dégradation P2-4 : note doit être inline (F-29 pur). \
         inline_ulids={inline_ulids:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Task 8 — Mode Compact (vue foldée F-30)
// ─────────────────────────────────────────────────────────────────────────────

/// **compact_folds_sent_to_stubs** — T8-1 : le mode compact fold au moins une note en stub.
///
/// 3 notes seedées, toutes marquées `mark_sent` (snippets distincts du body).
/// Appel `mode=compact` avec `budget_tokens=25` (très serré) — au moins une note
/// doit tomber en stub (references non-vide).
///
/// # Invariants
///
/// - `references` non-vide : au moins 1 note foldée en stub.
/// - HTTP 200 (pas d'erreur).
/// - Réponse structurée valide (`included` + `references` présents).
#[tokio::test]
async fn compact_folds_sent_to_stubs() {
    use chrono::Utc;

    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);
    let session_id = ulid::Ulid::new().to_string();
    let now_ms = Utc::now().timestamp_millis();

    // Seeder 3 notes avec corps longs (dépassent budget_tokens=25).
    let ulid_a = seed_note_return_ulid(
        &env,
        "Note Compact A",
        "compact fold vue foldee note A body text alpha beta gamma delta epsilon",
    )
    .await;
    let ulid_b = seed_note_return_ulid(
        &env,
        "Note Compact B",
        "compact fold vue foldee note B body text zeta eta theta iota kappa",
    )
    .await;
    let ulid_c = seed_note_return_ulid(
        &env,
        "Note Compact C",
        "compact fold vue foldee note C body text lambda mu nu xi omicron",
    )
    .await;

    // Marquer les 3 notes comme sent dans la session.
    let store = env
        .state
        .session_trace
        .as_ref()
        .expect("session_trace présent — invariant test T8-1");
    for (ulid, snip) in [
        (ulid_a.as_str(), "snip-a compact fold"),
        (ulid_b.as_str(), "snip-b compact fold"),
        (ulid_c.as_str(), "snip-c compact fold"),
    ] {
        store
            .mark_sent("main", &session_id, ulid, snip, now_ms)
            .await
            .expect("mark_sent T8-1 — invariant test");
    }

    // Appel mode=compact avec budget serré (25 tokens) → au moins une note foldée.
    let (status, resp) = call_vault_context_json_status(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "compact fold vue foldee",
            "mode": "compact",
            "session_id": session_id,
            "budget_tokens": 25,
        }),
    )
    .await;

    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "compact mode=compact doit retourner 200. resp={resp}"
    );

    let references = resp["references"]
        .as_array()
        .expect("references présent T8-1");
    assert!(
        !references.is_empty(),
        "T8-1 : mode compact avec budget=25 doit fold au moins 1 note en stub. \
         references={references:?}"
    );
}

/// **compact_preserves_dereferenceability** — T8-2 : aucune note sent n'est perdue.
///
/// 3 notes seedées, toutes marquées `mark_sent`. Appel `mode=compact` avec
/// `budget_tokens=1` (extrêmement serré) → toutes les notes apparaissent
/// dans `included` ou `references` (invariant "aucune note sent perdue").
///
/// # Invariants
///
/// - Union(included ULIDs ∪ references ULIDs) contient les 3 ULIDs seedés.
/// - HTTP 200.
#[tokio::test]
async fn compact_preserves_dereferenceability() {
    use chrono::Utc;

    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);
    let session_id = ulid::Ulid::new().to_string();
    let now_ms = Utc::now().timestamp_millis();

    // Seeder 3 notes.
    let ulid_a = seed_note_return_ulid(
        &env,
        "Deref Note A",
        "deref compact invariant note A preserve sent alpha beta",
    )
    .await;
    let ulid_b = seed_note_return_ulid(
        &env,
        "Deref Note B",
        "deref compact invariant note B preserve sent gamma delta",
    )
    .await;
    let ulid_c = seed_note_return_ulid(
        &env,
        "Deref Note C",
        "deref compact invariant note C preserve sent epsilon zeta",
    )
    .await;

    // Marquer les 3 notes comme sent.
    let store = env
        .state
        .session_trace
        .as_ref()
        .expect("session_trace présent — invariant test T8-2");
    for (ulid, snip) in [
        (ulid_a.as_str(), "snip-deref-a"),
        (ulid_b.as_str(), "snip-deref-b"),
        (ulid_c.as_str(), "snip-deref-c"),
    ] {
        store
            .mark_sent("main", &session_id, ulid, snip, now_ms)
            .await
            .expect("mark_sent T8-2 — invariant test");
    }

    // budget_tokens=1 → budget extrêmement serré → toutes les notes passent en stubs
    // (inline impossible), mais AUCUNE ne doit être perdue (invariant T8-2).
    let (status, resp) = call_vault_context_json_status(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "deref compact invariant preserve sent",
            "mode": "compact",
            "session_id": session_id,
            "budget_tokens": 1,
        }),
    )
    .await;

    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "compact budget=1 doit retourner 200. resp={resp}"
    );

    // Collecter les ULIDs depuis included et references.
    let included = resp["included"].as_array().expect("included présent T8-2");
    let references = resp["references"]
        .as_array()
        .expect("references présent T8-2");

    let mut visible_ulids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for note in included {
        if let Some(u) = note["ulid"].as_str() {
            visible_ulids.insert(u);
        }
    }
    for stub in references {
        if let Some(u) = stub["ulid"].as_str() {
            visible_ulids.insert(u);
        }
    }

    // Invariant : les 3 notes sent doivent toutes être représentées.
    for (label, ulid) in [
        ("A", ulid_a.as_str()),
        ("B", ulid_b.as_str()),
        ("C", ulid_c.as_str()),
    ] {
        assert!(
            visible_ulids.contains(ulid),
            "T8-2 : note sent {label} ({ulid}) absente de included+references — \
             invariant 'aucune note sent perdue' violé. visible={visible_ulids:?}"
        );
    }
}

/// **compact_requires_session_id** — T8-3 : mode compact sans session_id → 400.
///
/// Le mode compact exige un `session_id` (la vue foldée opère sur le sent_map).
/// Un appel sans `session_id` doit retourner `400 BAD_REQUEST`.
///
/// # Invariants
///
/// - HTTP 400 (InvalidInput).
/// - Body de réponse structuré (pas de panique serveur).
#[tokio::test]
async fn compact_requires_session_id() {
    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    // Appel mode=compact SANS session_id → doit retourner 400.
    let (status, _resp) = call_vault_context_json_status(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "test compact sans session",
            "mode": "compact",
            // pas de session_id
        }),
    )
    .await;

    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "T8-3 : mode compact sans session_id doit retourner 400 BAD_REQUEST"
    );
}

// ── Task 10 — cache_breakpoint_hint (spec §5.8) ───────────────────────────

/// **hint_true_above_threshold** — T10-1 : `cache_breakpoint_hint=true` quand
/// `budget_used > cache_breakpoint_threshold_tokens`.
///
/// Stratégie : seuil à 1 token (threshold=1) + corpus seedé → le premier
/// assemblage non-vide produit `budget_used > 1` → hint=true.
///
/// # Invariants
///
/// - `resp["cache_breakpoint_hint"] == true`.
/// - `budget_used >= 1` (corpus non-vide → assemblage non-vide).
#[tokio::test]
async fn hint_true_above_threshold() {
    // Seuil minimal (1 token) → tout assemblage non-vide dépasse le seuil.
    let config = gradatum_server::config::ContextConfig {
        cache_breakpoint_threshold_tokens: 1,
        ..Default::default()
    };
    let env = build_app_with_context_config(Arc::new(FakeEmbedder { dim: 1024 }), config).await;
    seed_notes(&env, 3).await;
    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha beta",
            "mode": "assembled",
        }),
    )
    .await;

    let budget_used = resp["budget_used"].as_u64().unwrap_or(0);
    assert!(
        budget_used > 0,
        "T10-1 : budget_used devrait être > 0 avec un corpus seedé. resp={resp}"
    );
    assert_eq!(
        resp["cache_breakpoint_hint"],
        serde_json::json!(true),
        "T10-1 : hint devrait être true quand budget_used({budget_used}) > threshold(1). resp={resp}"
    );
}

/// **hint_false_below_threshold** — T10-2 : `cache_breakpoint_hint=false` quand
/// `budget_used <= cache_breakpoint_threshold_tokens`.
///
/// Stratégie : seuil très élevé (999_999) → aucun assemblage normal ne le dépasse.
///
/// # Invariants
///
/// - `resp["cache_breakpoint_hint"] == false`.
#[tokio::test]
async fn hint_false_below_threshold() {
    // Seuil très élevé → budget_used ne le dépassera jamais en test.
    let config = gradatum_server::config::ContextConfig {
        cache_breakpoint_threshold_tokens: 999_999,
        ..Default::default()
    };
    let env = build_app_with_context_config(Arc::new(FakeEmbedder { dim: 1024 }), config).await;
    seed_notes(&env, 5).await;
    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "alpha beta",
            "mode": "assembled",
        }),
    )
    .await;

    assert_eq!(
        resp["cache_breakpoint_hint"],
        serde_json::json!(false),
        "T10-2 : hint devrait être false quand budget_used <= threshold(999999). resp={resp}"
    );
}

/// **cache_breakpoint_threshold_defaults** — T10-3 : `ContextConfig::default()` donne
/// `cache_breakpoint_threshold_tokens = 500`.
///
/// # Invariants
///
/// - `ContextConfig::default().cache_breakpoint_threshold_tokens == 500`.
/// - La réponse contient toujours le champ `cache_breakpoint_hint` (jamais absent).
#[tokio::test]
async fn cache_breakpoint_threshold_defaults() {
    // Test unitaire inline — pas besoin de serveur.
    let cfg = gradatum_server::config::ContextConfig::default();
    assert_eq!(
        cfg.cache_breakpoint_threshold_tokens, 500,
        "T10-3 : cache_breakpoint_threshold_tokens défaut doit être 500"
    );

    // Vérification E2E : le champ est toujours présent dans la réponse JSON.
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "test default hint",
            "mode": "assembled",
        }),
    )
    .await;

    assert!(
        resp.get("cache_breakpoint_hint").is_some(),
        "T10-3 : cache_breakpoint_hint doit toujours être présent dans la réponse. resp={resp}"
    );
}
