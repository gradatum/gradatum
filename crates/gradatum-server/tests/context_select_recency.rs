//! Tests TDD M-1 — recency de `vault_context` alignée sur `anchor_ms` (parité `vault_search`).
//!
//! ## Problème (M-1, P2)
//!
//! `vault_search` calcule la recency sur `anchor_ms` (F-17, v0.7.4).
//! `vault_context` (`select_budget_aware`) utilisait encore `recency_factor(created_ms, now)`
//! → divergence de ranking pour les notes `occurred_at`/Event dont anchor ≠ created.
//!
//! ## Couverture
//!
//! 1. `context_recency_uses_anchor_ms_not_created_ms` — RED avant fix.
//!    Note Event (anchor = 2 ans, created = maintenant) → recency basée sur anchor (bas) → score < 1.01.
//!    Avant fix : recency basée sur created (récent) → score ≈ 1.2 → FAIL.
//!
//! 2. `context_recency_created_anchor_fallback_bit_identical` — non-régression.
//!    Note sans `temporal_index` → fallback created_ms → score ≈ 1.2 (inchangé avant/après fix).
//!
//! 3. `context_trust_age_uses_created_ms_not_anchor_ms` — régression guard.
//!    Note trust/provenance="distilled" + anchor 2 ans + created récent → age_days ≈ 0 →
//!    trust decay quasi-nulle → score > 1.1. Si age_days bascule sur anchor_ms → score < 1.1.
//!
//! ## Architecture du harness
//!
//! `select_budget_aware` est appelé directement (pas via HTTP) pour contrôler les inputs
//! précisément (now_ms, rrf_score, candidats, budget large). Harness via helpers::build_app_with_embedder.

use std::sync::Arc;

use chrono::Utc;
use gradatum_core::identity::NoteId;
use gradatum_core::index::{AnchorSrc, TemporalEntry};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_search::resolve_weights;
use gradatum_server::context::{
    retrieval::Candidate, select::select_budget_aware, tokens::HeuristicEstimator,
};
use ulid::Ulid;

#[path = "helpers/mod.rs"]
mod helpers;

use helpers::{FakeEmbedder, build_app_with_embedder};

/// Test 1 (RED avant fix, GREEN après fix) — `vault_context` recency basée sur `anchor_ms`.
///
/// Setup :
/// - Note seedée récemment (`created_ms` ≈ now via `seed_note_with_fts`).
/// - Entrée `temporal_index` : `anchor_ms` = 2 ans dans le passé (`AnchorSrc::OccurredAt`).
///
/// Attendu post-fix :
/// `recency_factor(anchor_ms, now)` = `exp(-0.01 × 730)` ≈ 0.0007.
/// Score = `1.0 × (1 + 0.2 × 0.0007) × 1.0 = 1.00014` → assert `score < 1.01` PASSE.
///
/// Avant fix (RED) :
/// `recency_factor(created_ms, now)` = `exp(0)` = 1.0.
/// Score ≈ 1.2 → assert `score < 1.01` ÉCHOUE → test rouge confirmé.
#[tokio::test]
async fn context_recency_uses_anchor_ms_not_created_ms() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let idx = env._vault_typed.index();
    let now_ms = Utc::now().timestamp_millis();
    // 2 ans en ms (2 × 365 × 86_400_000 ms).
    let two_years_ago_ms = now_ms - 2 * 365 * 86_400_000_i64;

    // Seed la note dans l'index SQLite (created = now via seed_note_with_fts).
    let ulid = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &ulid,
        "reference",
        "# Événement Ancien M1\ncorps recency test m1 — ancre 2 ans.",
    )
    .await
    .expect("seed_note_with_fts — invariant test M-1");

    // Insérer l'ancre temporelle 2 ans dans le passé (occurred_at).
    // anchor_ms ≠ created_ms → scénario M-1 (divergence pré-fix).
    let entry = TemporalEntry {
        note_id: ulid.clone(),
        vault_id: "main".to_string(),
        anchor_ms: two_years_ago_ms,
        anchor_src: AnchorSrc::OccurredAt,
        doc_kind: "Event".to_string(),
        valid_until_ms: None,
    };
    // write_temporal_entry via IndexStore (implémenté par SqliteIndex via Arc<dyn Index>).
    env.state
        .search
        .write_temporal_entry(&entry)
        .await
        .expect("write_temporal_entry — invariant test M-1");

    let candidate = Candidate {
        note_id: ulid.clone(),
        rrf_score: 1.0,
    };
    let weights = resolve_weights(None);
    let estimator = HeuristicEstimator;

    let (selected, _stubs, _budget_used) = select_budget_aware(
        &env.state,
        "main",
        vec![candidate],
        &weights,
        &estimator,
        10_000, // budget large → note passe inline
        0,
        now_ms,
    )
    .await
    .expect("select_budget_aware — invariant test M-1");

    assert_eq!(
        selected.len(),
        1,
        "1 note doit être sélectionnée (M-1 test 1)"
    );
    let score = selected[0].score;

    // Post-fix : recency = recency_factor(anchor_ms = 2 ans, now) ≈ 0.0007
    // → score = 1.0 × (1 + 0.2 × 0.0007) × 1.0 ≈ 1.00014 → PASSE.
    // Avant fix (RED) : recency = recency_factor(created_ms = now, now) = 1.0
    // → score ≈ 1.2 → ÉCHOUE (score < 1.01 = false).
    assert!(
        score < 1.01,
        "M-1 : recency doit être basée sur anchor_ms (2 ans) → score ≈ 1.00014 \
         (got {score:.6}). Score ≈ 1.2 indique que created_ms (récent) est encore \
         utilisé — bug pré-fix M-1."
    );
}

/// Test 2 (non-régression) — fallback `created_ms` quand `temporal_index` absent.
///
/// Note sans entrée `temporal_index` → `anchor_ms = None` → fallback `created_ms`.
/// Score identique à l'ancien comportement (recency haute, note créée récemment).
/// Passe avant et après le fix.
#[tokio::test]
async fn context_recency_created_anchor_fallback_bit_identical() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let idx = env._vault_typed.index();
    let now_ms = Utc::now().timestamp_millis();

    // Note statique (sans temporal_index) — cas le plus fréquent (99.7% du corpus).
    let ulid = Ulid::new().to_string();
    idx.seed_note_with_fts(
        &ulid,
        "reference",
        "# Note Statique M1\ncorps fallback created_ms — pas d'anchor.",
    )
    .await
    .expect("seed_note_with_fts — invariant test M-1 fallback");

    // Aucune insertion temporal_index → anchor_ms = None → fallback created_ms.

    let candidate = Candidate {
        note_id: ulid.clone(),
        rrf_score: 1.0,
    };
    let weights = resolve_weights(None);
    let estimator = HeuristicEstimator;

    let (selected, _stubs, _budget_used) = select_budget_aware(
        &env.state,
        "main",
        vec![candidate],
        &weights,
        &estimator,
        10_000,
        0,
        now_ms,
    )
    .await
    .expect("select_budget_aware — invariant test M-1 fallback");

    assert_eq!(
        selected.len(),
        1,
        "1 note doit être sélectionnée (M-1 test 2)"
    );
    let score = selected[0].score;

    // created_ms = now (seed_note_with_fts → Utc::now()) → recency_factor(now, now) = 1.0
    // → score = 1.0 × (1 + 0.2 × 1.0) × 1.0 = 1.2 (trust=None, in_degree=0).
    // Valide avant ET après le fix : fallback created_ms préserve le score.
    assert!(
        score > 1.15,
        "M-1 fallback : note créée récemment sans temporal_index → score ≈ 1.2 \
         (got {score:.6}). Score < 1.15 indique une régression du fallback."
    );
}

/// Test 3 (régression guard) — `trust age_days` reste sur `created_ms`, pas `anchor_ms`.
///
/// Setup :
/// - Note avec `provenance = "distilled"` (half_life 90j), `trust = 0.9`.
/// - `created_ms` ≈ now (recent ingestion via `seed_note_with_status`).
/// - `anchor_ms` = 2 ans dans le passé.
///
/// Attendu (correct) :
/// `age_days` = `(now - created_ms) / 86_400_000` ≈ 0 jours → decay quasi-nulle →
/// `trust_decayed` ≈ 0.9 → facteur trust `(1 + 0.15 × 0.9)` = 1.135 → `score > 1.1`.
///
/// Régression (si `age_days` utilisait `anchor_ms`) :
/// `age_days` = 730 jours → `trust_decayed = 0.9 × 0.5^(730/90)` ≈ 0.0032 →
/// facteur trust ≈ 1.0005 → `score < 1.1` → test ÉCHOUE → détecte la régression.
#[tokio::test]
async fn context_trust_age_uses_created_ms_not_anchor_ms() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let idx = env._vault_typed.index();
    let now_ms = Utc::now().timestamp_millis();
    let two_years_ago_ms = now_ms - 2 * 365 * 86_400_000_i64;

    // Seed avec provenance="distilled" (active le half_life 90j dans TrustDecayConfig).
    // created = now (seed_note_with_status → Utc::now()) → age_days correct ≈ 0.
    let ulid = Ulid::new().to_string();
    idx.seed_note_with_status(
        &ulid,
        Section::Reference,
        "corps trust age test m1.",
        NoteStatus::Live,
        Some("distilled"),
    )
    .await
    .expect("seed_note_with_status — invariant test M-1 trust");

    // Fixer trust = 0.9 via trait IndexStore (délégué à SqliteIndex).
    let nid = NoteId(Ulid::from_string(&ulid).expect("ULID parse — invariant test M-1 trust"));
    env.state
        .search
        .set_note_trust("main", &nid, 0.9)
        .await
        .expect("set_note_trust — invariant test M-1 trust");

    // Anchor 2 ans dans le passé (anchor ≠ created → scénario M-1).
    let entry = TemporalEntry {
        note_id: ulid.clone(),
        vault_id: "main".to_string(),
        anchor_ms: two_years_ago_ms,
        anchor_src: AnchorSrc::OccurredAt,
        doc_kind: "Event".to_string(),
        valid_until_ms: None,
    };
    env.state
        .search
        .write_temporal_entry(&entry)
        .await
        .expect("write_temporal_entry — invariant test M-1 trust");

    let candidate = Candidate {
        note_id: ulid.clone(),
        rrf_score: 1.0,
    };
    let weights = resolve_weights(None);
    let estimator = HeuristicEstimator;

    let (selected, _stubs, _budget_used) = select_budget_aware(
        &env.state,
        "main",
        vec![candidate],
        &weights,
        &estimator,
        10_000,
        0,
        now_ms,
    )
    .await
    .expect("select_budget_aware — invariant test M-1 trust");

    assert_eq!(
        selected.len(),
        1,
        "1 note doit être sélectionnée (M-1 test 3)"
    );
    let score = selected[0].score;

    // Correct (age_days sur created_ms = now → age_days ≈ 0) :
    //   trust_decayed = 0.9 × 0.5^(0/90) = 0.9
    //   facteur trust = (1 + 0.15 × 0.9) = 1.135
    //   score ≈ 1.00014 × 1.135 ≈ 1.1353 > 1.1 → PASSE.
    //
    // Régression (age_days sur anchor_ms = 730j) :
    //   trust_decayed = 0.9 × 0.5^(730/90) ≈ 0.0032
    //   facteur trust ≈ 1.000478
    //   score ≈ 1.00014 × 1.000478 ≈ 1.0006 < 1.1 → ÉCHOUE.
    assert!(
        score > 1.1,
        "M-1 trust : age_days basé sur created_ms (recent → 0j) → trust decay quasi-nulle → \
         score > 1.1 (got {score:.6}). Score < 1.1 indique que age_days utilise anchor_ms \
         (régression — trust âge doit rester sur created_ms)."
    );
}
