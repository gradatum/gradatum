//! Task 10 — Harness d'évaluation : preuve stricte d'apport RRF+composite vs baseline FTS-only.
//!
//! ## Objectif
//!
//! Mesurer Δ recall@k entre le pipeline complet (BM25 + sémantique via RRF) et une
//! baseline FTS-only (BM25 pur), sur un jeu de requêtes labellisées incluant des
//! **requêtes paraphrase** (zéro overlap lexical query↔corps de note).
//!
//! ## Garantie anti-tautologie
//!
//! - [`eval::EvalEmbedder`] mappe TEXT→TOPIC (pas par identité de note).
//! - Keywords note-side / query-side **disjoints** → BM25 ne peut PAS retrouver les
//!   notes cibles pour les requêtes paraphrase.
//! - Sans `store_embeddings=true`, `search_semantic` retourne ∅ → Δ=0 → assertion échoue
//!   (red-test démontrable sans toucher au code prod).
//!
//! ## Modes
//!
//! | Env | Embedder | BM25 | Sémantique | Embeddings stockés |
//! |---|---|---|---|---|
//! | `assembled_env` | `EvalEmbedder` | oui | oui | oui (`store_embeddings=true`) |
//! | `baseline_env`  | `NoopBackend`  | oui | non | non |
//!
//! ## Assertion
//!
//! `assembled.recall@5 > baseline.recall@5` (strict `>`, pas `>=`).
//! La différence provient des 5 requêtes paraphrase : baseline.recall=0, assembled.recall>0.

#[path = "eval/mod.rs"]
mod eval;
#[path = "helpers/mod.rs"]
mod helpers;

use std::sync::Arc;

use eval::{EvalEmbedder, run_eval, seed_eval_corpus};
use helpers::{build_app, build_app_with_embedder};

/// Preuve que le pipeline Assembled (RRF BM25+sémantique) bat strictement la baseline FTS-only
/// sur les requêtes paraphrase (zéro overlap lexical).
///
/// ## TDD red-test
///
/// Sans `seed_eval_corpus(idx, true)` (pas d'embeddings stockés) :
/// - `search_semantic` retourne ∅ même avec EvalEmbedder.
/// - `assembled.recall_at_k == baseline.recall_at_k` (pas de gain sémantique).
/// - L'assertion `assembled > baseline` ÉCHOUE → preuve du rouge TDD.
///
/// ## Green avec embeddings stockés
///
/// Avec `store_embeddings=true` :
/// - `search_semantic` retrouve les notes du même topic via cosine(centroïde, centroïde) = 1.0.
/// - Pour les 5 requêtes paraphrase : assembled.recall > 0, baseline.recall = 0.
/// - `assembled.recall@5 > baseline.recall@5` → assertion passe.
#[tokio::test]
async fn assembled_strictly_beats_fts_baseline_on_paraphrase_queries() {
    const K: usize = 5;

    // ── Env Assembled : EvalEmbedder + embeddings stockés ────────────────────
    let assembled_env = build_app_with_embedder(Arc::new(EvalEmbedder)).await;
    let idx_assembled = assembled_env._vault_typed.index();
    let key_to_ulid_assembled = seed_eval_corpus(idx_assembled, true).await;

    // ── Env FtsOnly (baseline) : NoopBackend, PAS d'embeddings ───────────────
    // `build_app()` utilise `NoopBackend` (embed_fallback=true → BM25-only).
    let baseline_env = build_app().await;
    let idx_baseline = baseline_env._vault_typed.index();
    // `store_embeddings=false` : même corpus seedé FTS, sans vecteurs en DB.
    let key_to_ulid_baseline = seed_eval_corpus(idx_baseline, false).await;

    // ── Évaluation ───────────────────────────────────────────────────────────
    let assembled = run_eval(&assembled_env.state, &key_to_ulid_assembled, K).await;
    let baseline = run_eval(&baseline_env.state, &key_to_ulid_baseline, K).await;

    // Affichage des métriques (visible avec --no-capture).
    println!(
        "Assembled : precision@{K}={:.3}  recall@{K}={:.3}  (n={})",
        assembled.precision_at_k, assembled.recall_at_k, assembled.n_queries
    );
    println!(
        "Baseline  : precision@{K}={:.3}  recall@{K}={:.3}  (n={})",
        baseline.precision_at_k, baseline.recall_at_k, baseline.n_queries
    );
    println!(
        "Δ recall@{K} = {:.3} | Δ precision@{K} = {:.3}",
        assembled.recall_at_k - baseline.recall_at_k,
        assembled.precision_at_k - baseline.precision_at_k,
    );

    // ── Assertion stricte (> pas >=) ─────────────────────────────────────────
    assert!(
        assembled.recall_at_k > baseline.recall_at_k,
        "apport non démontré : Assembled recall@{K} ({:.3}) <= baseline ({:.3}).\n\
         Vérifier :\n\
         1. Les embeddings sont bien stockés via seed_note_embedding (store_embeddings=true).\n\
         2. search_semantic retourne des hits (debug: vault_id='main', embedder_id='eval-embedder-v1').\n\
         3. detect_topic reconnaît les keywords query-side des requêtes paraphrase.",
        assembled.recall_at_k,
        baseline.recall_at_k,
    );
}
