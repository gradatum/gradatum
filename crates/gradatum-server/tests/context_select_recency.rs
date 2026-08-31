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
//!    Note section `debug` (doc_kind Event, F-261 trust dérivé = 0.40) + anchor 2 ans +
//!    created récent → age_days ≈ 0 → trust decay quasi-nulle → score > 1.05.
//!    Si age_days bascule sur anchor_ms → score < 1.05.
//!
//! ## Architecture du harness
//!
//! `select_budget_aware` est appelé directement (pas via HTTP) pour contrôler les inputs
//! précisément (now_ms, rrf_score, candidats, budget large). Harness via helpers::build_app_with_embedder.

use std::sync::Arc;

use chrono::Utc;
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
    let ulid = Ulid::generate().to_string();
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
    // → facteur recency (1 + 0.2 × 0.0007) ≈ 1.00014.
    // F-261 : la note est en section "reference" → trust dérivé = 0.60, doc_kind Static
    // (pas de decay) → facteur trust (1 + 0.15 × 0.60) = 1.09.
    // → score = 1.0 × 1.00014 × 1.09 ≈ 1.09015 < 1.2 → PASSE.
    // Avant fix (RED) : recency = recency_factor(created_ms = now, now) = 1.0
    // → score ≈ 1.2 × 1.09 ≈ 1.308 → ÉCHOUE (score < 1.2 = false).
    assert!(
        score < 1.2,
        "M-1 : recency doit être basée sur anchor_ms (2 ans) → score ≈ 1.09015 \
         (got {score:.6}). Score ≈ 1.3 indique que created_ms (récent) est encore \
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
    let ulid = Ulid::generate().to_string();
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
/// Setup (F-261 — trust dérivé de la section, decay par doc_kind) :
/// - Note en section `debug` (doc_kind `Event` → half_life 90j), `trust` dérivé = 0.40.
/// - `created_ms` ≈ now (recent ingestion via `seed_note_with_status`).
/// - `anchor_ms` = 2 ans dans le passé (anchor ≠ created → scénario M-1).
///
/// Attendu (correct) :
/// `age_days` = `(now - created_ms) / 86_400_000` ≈ 0 jours → decay quasi-nulle →
/// `trust_decayed` ≈ 0.40 → facteur trust `(1 + 0.15 × 0.40)` = 1.06.
/// La recency, elle, suit l'ancre (2 ans) → `(1 + 0.2 × recency_factor(2ans))` ≈ 1.00014.
/// → score ≈ 1.0 × 1.00014 × 1.06 ≈ 1.0601 > 1.05.
///
/// Régression (si `age_days` utilisait `anchor_ms`) :
/// `age_days` = 730 jours → `trust_decayed = 0.40 × 0.5^(730/90)` ≈ 0.00145 →
/// facteur trust ≈ 1.0002 → score ≈ 1.00014 × 1.0002 ≈ 1.00034 < 1.05 →
/// test ÉCHOUE → détecte la régression.
#[tokio::test]
async fn context_trust_age_uses_created_ms_not_anchor_ms() {
    let env = build_app_with_embedder(Arc::new(FakeEmbedder { dim: 8 })).await;
    let idx = env._vault_typed.index();
    let now_ms = Utc::now().timestamp_millis();
    let two_years_ago_ms = now_ms - 2 * 365 * 86_400_000_i64;

    // Seed en section debug → doc_kind "Event" → half_life 90j activé.
    // created = now (seed_note_with_status → Utc::now()) → age_days correct ≈ 0.
    // trust dérivé de la section = trust_for_section(Debug) = 0.40 (pas de set_note_trust :
    // F-261 le scoring ignore la colonne `notes.trust`).
    let ulid = Ulid::generate().to_string();
    idx.seed_note_with_status(
        &ulid,
        Section::Debug,
        "corps trust age test m1.",
        NoteStatus::Live,
        None,
    )
    .await
    .expect("seed_note_with_status — invariant test M-1 trust");

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
    //   trust_decayed = 0.40 × 0.5^(0/90) = 0.40
    //   facteur trust = (1 + 0.15 × 0.40) = 1.06
    //   recency (ancre 2 ans) → (1 + 0.2 × 0.0007) ≈ 1.00014
    //   score ≈ 1.00014 × 1.06 ≈ 1.0601 > 1.05 → PASSE.
    //
    // Régression (age_days sur anchor_ms = 730j) :
    //   trust_decayed = 0.40 × 0.5^(730/90) ≈ 0.00145
    //   facteur trust ≈ 1.0002
    //   score ≈ 1.00014 × 1.0002 ≈ 1.00034 < 1.05 → ÉCHOUE.
    assert!(
        score > 1.05,
        "M-1 trust : age_days basé sur created_ms (recent → 0j) → trust decay quasi-nulle → \
         score > 1.05 (got {score:.6}). Score < 1.05 indique que age_days utilise anchor_ms \
         (régression — trust âge doit rester sur created_ms)."
    );
}
