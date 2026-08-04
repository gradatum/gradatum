//! Harness évaluation apport Context Efficiency F-29/F-30 (v0.7.2).
//!
//! ## Objectif
//!
//! Mesure reproductible du gain des features Context Efficiency :
//!
//! 1. **F-29 — ratio stubs** : `reference_mode=true` + budget inline serré sur un
//!    corpus de 10 notes variées → stubs produits (économie tokens effective, ratio > 0).
//! 2. **F-30 — delta tokens** : deux tours dans la même session → tour T1 envoie les
//!    notes nouvelles inline, tour T2 force les already-sent en stubs → `budget_used`
//!    diminue (delta tokens ≥ 0).
//! 3. **Compact sans perte** : `mode=compact` sur la session → toutes les notes sent
//!    visibles dans `included ∪ references` (invariant déréférençabilité F-30).
//!
//! ## Style
//!
//! Pattern identique à `tests/proactive_eval.rs` (v0.7.1) :
//! - corpus seedé via `write_note_in_section` / `seed_note_return_ulid`.
//! - helpers `call_vault_context_json` / `call_vault_context_json_status`.
//! - assertions sanity sans seuils stricts : ratio > 0, delta ≥ 0, aucune perte.
//!
//! ## Setup
//!
//! `build_app_with_session_trace_and_embedder(FakeEmbedder { dim: 1024 })` :
//! session_trace requis pour F-30 et compact ; FakeEmbedder active le chemin sémantique
//! (non-Noop → embed_fallback=false → RRF + BM25, suffisant pour retrouver les notes
//! par FTS sur le contenu seedé).

#[path = "helpers/mod.rs"]
mod helpers;

use std::sync::Arc;

use helpers::{
    FakeEmbedder, build_app_with_session_trace_and_embedder, call_vault_context_json,
    call_vault_context_json_status, sign_token,
};

/// Corpus de 10 notes couvrant plusieurs sections et tailles variées.
///
/// Toutes contiennent les mots-clés `"efficiency"` et `"eval"` → matchées par les
/// requêtes des tests. Corps d'environ 15 mots → `estimate(body) ≈ 20 tokens` →
/// dépassent un budget inline serré de 25 tokens, facilitant la production de stubs F-29.
const CORPUS: &[(&str, &str, &str)] = &[
    (
        "decisions",
        "Décision Archi Gradatum F29 Eval",
        "efficiency f29 stub inline split budget decisions context eval harness alpha beta gamma",
    ),
    (
        "lessons-learned",
        "Leçon Rust Async Context F29",
        "efficiency f29 recall inline stub session trace leçon rust async context eval alpha beta",
    ),
    (
        "reasoning",
        "Raisonnement Context Tokens F29",
        "efficiency f29 tokens budget inline stub raisonnement context assembly eval alpha",
    ),
    (
        "decisions",
        "Décision Session Tracking F30 Eval",
        "efficiency f30 session tracking already sent inline stub decisions eval beta gamma",
    ),
    (
        "lessons-learned",
        "Leçon Compaction Compact F30",
        "efficiency f30 compaction compact fold stubs sent leçon eval alpha beta",
    ),
    (
        "reasoning",
        "Raisonnement Split Budget Aware",
        "efficiency f29 split budget aware inline stub drop tiebreaker ulid raisonnement eval",
    ),
    (
        "decisions",
        "Décision Reference Mode True Eval",
        "efficiency f29 reference mode true stubs references réponse decisions eval",
    ),
    (
        "lessons-learned",
        "Leçon Mark Sent Snippet Figé",
        "efficiency f30 mark sent snippet figé session trace leçon eval alpha beta",
    ),
    (
        "reasoning",
        "Raisonnement Fold Priority Gros",
        "efficiency f30 fold priority gros ancien compact raisonnement eval alpha beta",
    ),
    (
        "decisions",
        "Décision Cache Breakpoint Hint",
        "efficiency f30 cache breakpoint hint threshold tokens decisions eval alpha beta",
    ),
];

/// F-29 : `reference_mode=true` + budget serré → ratio stubs > 0 (économie effective).
///
/// ## Protocole
///
/// Seed 10 notes (corps ≈ 20 tokens chacune) + appel `reference_mode=true` +
/// `budget_tokens=25` → 1 note inline max, le reste en stubs.
///
/// ## Mesures
///
/// - `inline_count` : notes envoyées inline (ont consommé du `budget_used`).
/// - `stub_count`   : notes converties en stubs (corps non envoyé, snippet seulement).
/// - `ratio_stubs`  : `stub / (inline + stub)` — fraction de notes économisées.
///
/// ## Sanity
///
/// `stub_count >= 1` et `ratio_stubs > 0.0` : au moins une note mise en stub.
/// Aucun seuil strict sur l'amplitude — on mesure la présence de l'économie.
#[tokio::test]
async fn f29_reference_mode_stubs_ratio_above_zero() {
    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    // ── 1. Seed corpus 10 notes (sections variées) ───────────────────────────
    //
    // write_note_in_section : fichier .md + index SQLite → FTS-searchable.
    for &(section, title, body) in CORPUS {
        env.write_note_in_section(section, title, body).await;
    }

    // ── 2. Appel reference_mode=true + budget inline serré ──────────────────
    //
    // budget_tokens=25 : avec 10 notes de ≈ 20 tokens → 1 note inline max.
    // Les suivantes tombent en stubs (snippet borné) → pas inline.
    let resp = call_vault_context_json(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "efficiency f29 stub inline split budget",
            "mode": "assembled",
            "reference_mode": true,
            "budget_tokens": 25,
        }),
    )
    .await;

    // ── 3. Lecture des compteurs ─────────────────────────────────────────────
    let inline_count = resp["counts"]["inline"].as_u64().unwrap_or(0);
    let stub_count = resp["counts"]["stub"].as_u64().unwrap_or(0);
    let dropped_count = resp["counts"]["dropped"].as_u64().unwrap_or(0);
    let budget_used = resp["budget_used"].as_u64().unwrap_or(0);
    let references_len = resp["references"].as_array().map(|a| a.len()).unwrap_or(0);
    let total_seen = inline_count + stub_count;

    let ratio_stubs = if total_seen > 0 {
        stub_count as f64 / total_seen as f64
    } else {
        0.0
    };

    println!(
        "F-29 ratio stubs : inline={inline_count} stubs={stub_count} \
         dropped={dropped_count} total_seen={total_seen} ratio={ratio_stubs:.3} \
         budget_used={budget_used} references_len={references_len}"
    );

    // ── 4. Sanity : stub_count >= 1 et ratio > 0 ────────────────────────────
    assert!(
        stub_count >= 1,
        "F-29 : au moins 1 stub doit être produit avec budget_tokens=25 et 10 notes seedées.\n\
         Vérifier que le corpus matche la requête et que l'estimation ≈ 20 tokens/note force \
         des stubs.\n\
         inline={inline_count} stubs={stub_count} dropped={dropped_count} resp={resp}",
    );
    assert!(
        ratio_stubs > 0.0,
        "F-29 ratio stubs doit être > 0 : {stub_count}/{total_seen} = {ratio_stubs:.3}.\n\
         Vérifier budget_tokens=25 force des stubs sur le corpus de 10 notes.\n\
         resp={resp}",
    );
}

/// F-30 : deux tours en session → T2 utilise ≤ tokens de T1 (économie effective).
///
/// ## Protocole
///
/// - T1 : session vide → notes nouvelles → inline (budget large 4000 tokens).
///   `budget_used_t1 = X`.
/// - T2 : même session_id + même requête → already-sent forcées en stubs →
///   `budget_used_t2 ≤ X`. `delta_tokens = X − Y ≥ 0`.
///
/// ## Mesures
///
/// - `budget_used_t1` : tokens consommés par les notes inline T1.
/// - `budget_used_t2` : tokens consommés T2 (moins, car already-sent → stubs).
/// - `delta_tokens`   : réduction effective de tokens au 2e tour.
/// - `stub_t2`        : confirmation filtre session actif (≥ 1 stub forcé).
///
/// ## Sanity
///
/// `stub_t2 >= 1` : F-30 force des already-sent en stubs.
/// `budget_t2 <= budget_t1` : le 2e tour n'est jamais plus lourd que le 1er.
#[tokio::test]
async fn f30_session_two_tours_delta_tokens() {
    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);

    // ── 1. Seed corpus (5 premières notes du CORPUS) ─────────────────────────
    for &(section, title, body) in &CORPUS[..5] {
        env.write_note_in_section(section, title, body).await;
    }

    let session_id = ulid::Ulid::new().to_string();

    // Requête sur des tokens présents INDIVIDUELLEMENT dans plusieurs notes du CORPUS[..5].
    // "f29 f30" ensemble dans la même requête forcerait un AND FTS5 → aucune note
    // individuelle ne contient les deux → 0 candidats.
    // "efficiency eval inline stub" : notes 1-4 contiennent tous ces tokens.
    let payload = serde_json::json!({
        "query": "efficiency eval inline stub",
        "mode": "assembled",
        "reference_mode": true,
        "session_id": session_id,
        "budget_tokens": 4000,
    });

    // ── 2. Tour T1 : notes nouvelles → inline ────────────────────────────────
    let resp_t1 = call_vault_context_json(env.app.clone(), &token, payload.clone()).await;

    let budget_t1 = resp_t1["budget_used"].as_u64().unwrap_or(0);
    let inline_t1 = resp_t1["counts"]["inline"].as_u64().unwrap_or(0);
    let stub_t1 = resp_t1["counts"]["stub"].as_u64().unwrap_or(0);

    // Pré-condition T1 : au moins 1 note inline (mesure delta significative).
    assert!(
        inline_t1 >= 1,
        "F-30 T1 pré-condition : au moins 1 note inline requise (mesure delta).\n\
         inline_t1={inline_t1} stub_t1={stub_t1} budget_t1={budget_t1} resp_t1={resp_t1}",
    );

    // ── 3. Tour T2 : already-sent → stubs, budget réduit ────────────────────
    let resp_t2 = call_vault_context_json(env.app.clone(), &token, payload).await;

    let budget_t2 = resp_t2["budget_used"].as_u64().unwrap_or(0);
    let inline_t2 = resp_t2["counts"]["inline"].as_u64().unwrap_or(0);
    let stub_t2 = resp_t2["counts"]["stub"].as_u64().unwrap_or(0);

    // ── 4. Mesures ───────────────────────────────────────────────────────────
    let delta_tokens = budget_t1.saturating_sub(budget_t2);

    println!(
        "F-30 delta tokens : T1=[inline={inline_t1} stub={stub_t1} budget={budget_t1}] \
         T2=[inline={inline_t2} stub={stub_t2} budget={budget_t2}] delta={delta_tokens}",
    );

    // ── 5. Sanity ─────────────────────────────────────────────────────────────
    //
    // stub_t2 >= 1 : le filtre session F-30 force au moins 1 already-sent en stub.
    // budget_t2 <= budget_t1 : le 2e tour est aussi léger ou plus léger (pas plus lourd).
    assert!(
        stub_t2 >= 1,
        "F-30 : counts.stub_t2 doit être >= 1 — les notes inline T1 deviennent stubs T2.\n\
         inline_t1={inline_t1} stub_t1={stub_t1} stub_t2={stub_t2}\n\
         resp_t2={resp_t2}",
    );
    assert!(
        budget_t2 <= budget_t1,
        "F-30 : budget_t2 ({budget_t2}) doit être ≤ budget_t1 ({budget_t1}).\n\
         Les already-sent passent en stubs, pas inline → budget réduit.\n\
         delta={delta_tokens} resp_t2={resp_t2}",
    );
}

/// Compact : `mode=compact` → aucune note sent perdue (included ∪ references).
///
/// ## Protocole
///
/// - Seed 4 notes + marquer les 4 comme `sent` dans la session (simuler 2 tours).
/// - Appel `mode=compact` + `budget_tokens=1` (extrêmement serré) →
///   toutes doivent apparaître dans `included ∪ references`.
///
/// ## Sanity
///
/// Aucune note sent absente de `included ∪ references`.
/// Budget compact peut tout mettre en stubs ou garder quelques notes inline —
/// l'invariant de préservation tient dans les deux cas.
#[tokio::test]
async fn compact_mode_all_sent_visible() {
    use chrono::Utc;

    let env = build_app_with_session_trace_and_embedder(Arc::new(FakeEmbedder { dim: 1024 })).await;
    let token = sign_token(&env.state);
    let session_id = ulid::Ulid::new().to_string();
    let now_ms = Utc::now().timestamp_millis();

    // ── 1. Seed 4 notes (4 premiers items du corpus) ─────────────────────────
    //
    // Suffixes uniques (index i) pour garantir l'isolation lexicale intra-test.
    let mut sent_ulids: Vec<String> = Vec::new();
    for (i, &(section, title, body)) in CORPUS[..4].iter().enumerate() {
        let full_title = format!("{title} cmpct{i}");
        let full_body = format!("{body} compact mode eval invariant cmpct{i}");
        let nid = env
            .write_note_in_section(section, &full_title, &full_body)
            .await;
        sent_ulids.push(nid.to_string());
    }

    // ── 2. Marquer les 4 notes comme sent (simuler 2 tours précédents) ───────
    //
    // mark_sent insère une SessionTraceRow (action_type="context-sent", marker=snippet).
    // get_sent les retrouvera lors de l'appel compact.
    let store = env
        .state
        .session_trace
        .as_ref()
        .expect("session_trace présent — invariant test compact_mode_all_sent_visible");
    for (i, ulid) in sent_ulids.iter().enumerate() {
        store
            .mark_sent(
                "main",
                &session_id,
                ulid,
                &format!("snip-cmpct-{i}"),
                now_ms,
            )
            .await
            .expect("mark_sent — invariant test compact_mode_all_sent_visible");
    }

    // ── 3. Appel mode=compact (budget extrêmement serré) ─────────────────────
    //
    // budget_tokens=1 : forcer la plupart ou toutes les notes en stubs.
    // L'invariant T8-2 garantit qu'aucune note sent n'est perdue.
    let (status, resp) = call_vault_context_json_status(
        env.app.clone(),
        &token,
        serde_json::json!({
            "query": "efficiency compact mode eval invariant",
            "mode": "compact",
            "session_id": session_id,
            "budget_tokens": 1,
        }),
    )
    .await;

    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "mode=compact doit retourner 200. resp={resp}",
    );

    // ── 4. Collecter les ULIDs visibles (included ∪ references) ─────────────
    let included = resp["included"]
        .as_array()
        .expect("included présent — invariant réponse compact");
    let references = resp["references"]
        .as_array()
        .expect("references présent — invariant réponse compact");

    let mut visible: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for note in included {
        if let Some(u) = note["ulid"].as_str() {
            visible.insert(u);
        }
    }
    for stub in references {
        if let Some(u) = stub["ulid"].as_str() {
            visible.insert(u);
        }
    }

    println!(
        "Compact aucune note perdue : sent={} visible={} included={} references={}",
        sent_ulids.len(),
        visible.len(),
        included.len(),
        references.len(),
    );

    // ── 5. Sanity : toutes les notes sent doivent être visibles ──────────────
    for (i, ulid) in sent_ulids.iter().enumerate() {
        assert!(
            visible.contains(ulid.as_str()),
            "Compact : note sent [{i}] ({ulid}) absente de included+references.\n\
             Invariant préservation déréférençabilité violé.\n\
             visible_count={} resp={resp}",
            visible.len(),
        );
    }
}
