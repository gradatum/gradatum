//! Sélection budget-aware des notes candidates pour l'assemblage de contexte LLM.
//!
//! This module implements the budget-aware candidate selection stage of the `vault_context` pipeline:
//! weighted composite scoring + descending sort (ULID tiebreaker) + lazy body fetch bounded
//! by an inline budget, followed by a stub phase for remaining candidates.
//!
//! ## Algorithme (v0.7.2 — split inline/stub)
//!
//! ```text
//! candidates ──► score composite (recency × pagerank × trust) ──► tri (score↓, ULID↑)
//!                                                                         │
//!                           ┌──────────────────────────────────────────────┘
//!                           │  Stage A — inline budget:
//!                           │    get_note (body + section + title + date)
//!                           │    estimator.estimate(body) → tokens
//!                           │    budget_remaining -= tokens
//!                           │    si budget_remaining < 0 → stop inline, début stubs
//!                           │
//!                           │  Stage B — stub budget:
//!                           │    get_note pour extraire le snippet (120 chars max)
//!                           │    stub_from_selected → Stub compact byte-stable
//!                           │    si stub_budget_remaining < 0 → drop
//!                           └──► (Vec<Selected>, Vec<Stub>, budget_inline_used)
//! ```
//!
//! ## ULID tiebreaker (cache determinism)
//!
//! Le tri secondaire par `note_id` (ULID lexicographique croissant) garantit que deux
//! runs sur les mêmes candidats avec les mêmes scores produisent le même split
//! inline/stub. Sans ce tiebreaker, `sort_unstable_by` sur `f64` ex-aequo est
//! non-déterministe → cache bust potentiel.
//!
//! ## Lazy body fetch
//!
//! Le corps complet (`body_text`) n'est chargé que pour les notes **retenues** après tri
//! par score — pas pour tous les candidats. Les métadonnées légères
//! (`created_ms`, `in_degree`, `trust`, `provenance`) are loaded in the scoring step for
//! composite score computation.
//!
//! ## Conformité spec v0.7.0 / v0.7.2
//!
//! | Spec | Implémentation |
//! |---|---|
//! | P0-2 forme multiplicative | `composite_score_weighted` (poids généralisent α/β/γ) |
//! | P2-2 date ISO | `DateTime::<Utc>::from_timestamp_millis(created).to_rfc3339()` |
//! | §4 lazy body fetch | `get_note` uniquement pour les notes retenues (après tri) |
//! | now_ms paramètre | passé en argument (jamais `Utc::now()` dans la lib testable) |
//! | Stub budget | inline top-K → stubs → drop (tiebreaker ULID, snippet 120 chars) |

use chrono::{DateTime, Utc};
use gradatum_search::{ResolvedWeights, composite_score_weighted, pagerank_factor, recency_factor};

use crate::context::reference::{Stub, render_stub, stub_from_selected};
use crate::context::retrieval::Candidate;
use crate::context::tokens::TokenEstimator;
use crate::state::AppState;
use gradatum_core::error::GradatumError;

/// Maximum codepoints in a stub snippet (safety cap for memory consistency).
///
/// Bounded here (not in config) because this is a safety cap, not an
/// externally tunable business parameter.
const STUB_SNIPPET_CHARS: usize = 120;

/// Note retenue après sélection budget-aware.
///
/// Produced by [`select_budget_aware`]. Consumed by the assembler
/// to construct `IncludedNote` and the assembled text.
#[derive(Debug)]
pub struct Selected {
    /// ULID de la note (String canonique).
    pub note_id: String,
    /// Titre Markdown H1 (ULID de repli si absent).
    pub title: String,
    /// Section thématique (ex. `"decisions"`, `"reference"`).
    pub section: String,
    /// Date de création ISO 8601 UTC (ex. `"2026-06-26T12:00:00+00:00"`).
    pub date: String,
    /// Score composite pondéré — utilisé par l'assembleur pour `IncludedNote.score`.
    pub score: f64,
    /// Full Markdown body — loaded lazily for retained notes only.
    pub body: String,
}

/// Trie les candidats scorés par score décroissant, puis par ULID croissant (tiebreaker).
///
/// Le tiebreaker ULID garantit un ordre stable et déterministe sur les ex-aequo,
/// conforming to the cache determinism constraint.
fn sort_candidates_by_score_then_ulid(scored: &mut [(Candidate, f64)]) {
    scored.sort_unstable_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.note_id.cmp(&b.0.note_id))
    });
}

/// Sélectionne les notes candidates avec un budget tokens, un scoring composite pondéré,
/// et produit un split inline/stub/drop (F-29, v0.7.2).
///
/// ## Algorithme en trois phases
///
/// **Scoring** (lightweight metadata):
/// For each `Candidate`, loads `(created_ms, in_degree)` via
/// `get_note_created_and_indegree` and `(trust, provenance)` via `get_trust_and_provenance`,
/// computes `composite_score_weighted(rrf_score, recency, pagerank, trust_params, weights)`.
/// Candidates whose metadata fetch fails are silently ignored (warn log).
///
/// **Inline selection** (lazy body fetch):
/// After descending score sort (ULID tiebreaker for cache determinism), for each
/// candidate loads `get_note` (body + section + title + ISO date) and accumulates
/// estimated tokens until exceeding `budget` → stop.
///
/// **Stub phase**:
/// For remaining candidates, loads `get_note` only to extract the snippet
/// (120 codepoints max, char-safe). Each stub is estimated in tokens
/// (`estimator.estimate(&render_stub(&stub))`) and accumulates until `stub_budget` → drop.
///
/// # Parameters
///
/// - `state`: app state (accès à `state.search` et `state.scoring`).
/// - `tenant`: vault tenant identifier.
/// - `candidates`: liste de candidats RRF issus de [`crate::context::retrieval::retrieve_candidates`].
/// - `weights`: poids résolus produits par [`gradatum_search::resolve_weights`].
/// - `estimator`: estimateur de tokens (ex. [`crate::context::tokens::HeuristicEstimator`]).
/// - `budget`: budget tokens maximal pour le mode inline (exclusif — stop dès dépassement).
/// - `stub_budget`: budget tokens maximal pour les stubs. Valeur 0 → pas de stubs (drop).
/// - `now_ms`: timestamp courant en epoch ms (paramètre pour testabilité).
///
/// # Returns
///
/// `(Vec<Selected>, Vec<Stub>, budget_inline_used)` :
/// - notes inline triées par score décroissant (tiebreaker ULID) + tokens inline utilisés.
/// - stubs des candidats hors budget inline (score décroissant, tiebreaker ULID conservé).
///
/// # Errors
///
/// Infaillible sur erreurs de fetch de notes individuelles (ignorées via warn log).
/// Retourne `GradatumError` uniquement si une erreur SQL systémique non récupérable survient.
///
/// # Side effects
///
/// Log `warn` pour chaque note dont le fetch de métadonnées ou de body échoue.
// La signature compte 8 arguments : la fonction existante était déjà à 7 (limite clippy).
// L'ajout de `stub_budget` est minimal et orthogonal aux autres paramètres — pas de struct
// wrapper pour éviter une abstraction YAGNI (dev-code-economy). Dès que les Tasks 3-7
// câblent la feature complète, reconsidérer un refactor en builder si ≥3 paramètres ajoutés.
#[expect(
    clippy::too_many_arguments,
    reason = "stub_budget est orthogonal aux 7 autres params existants — wrapper YAGNI pré-Tasks 3-7"
)]
pub async fn select_budget_aware(
    state: &AppState,
    tenant: &str,
    candidates: Vec<Candidate>,
    weights: &ResolvedWeights,
    estimator: &dyn TokenEstimator,
    budget: u32,
    stub_budget: u32,
    now_ms: i64,
) -> Result<(Vec<Selected>, Vec<Stub>, u32), GradatumError> {
    // ── Phase 1 : calcul du score composite pour chaque candidat ───────────────
    //
    // On charge les métadonnées légères (created_ms, in_degree, trust, provenance)
    // pour calculer le score composite. Les notes absentes ou en erreur sont ignorées.
    let mut scored: Vec<(Candidate, f64)> = Vec::with_capacity(candidates.len());

    // M-1 (fix recency anchor) : récupérer anchor_ms pour tous les candidats en 1 appel
    // SQL batch avant la boucle. anchor_ms = date d'événement (occurred_at/event_date) ;
    // fallback created_ms si la note n'a pas d'entrée dans temporal_index.
    // Parité sémantique avec vault_search (F-17, v0.7.4, logic.rs:407).
    let candidate_ids: Vec<String> = candidates.iter().map(|c| c.note_id.clone()).collect();
    let anchor_ms_map = state
        .search
        .get_anchor_ms_batch(tenant, &candidate_ids)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                err = %e,
                count = candidate_ids.len(),
                "select_budget_aware: get_anchor_ms_batch failed — fallback created_ms pour recency"
            );
            std::collections::HashMap::new()
        });

    for c in candidates {
        // Récupérer created_ms + in_degree (1 appel SQL léger).
        let (created_ms, in_degree) = match state
            .search
            .get_note_created_and_indegree(tenant, &c.note_id)
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    note_id = %c.note_id,
                    "select_budget_aware: get_note_created_and_indegree failed, note ignorée"
                );
                continue;
            }
        };

        // Récupérer trust + provenance pour le decay trust (default impl retourne (None, None)).
        let (trust_opt, provenance_opt) = state
            .search
            .get_trust_and_provenance(tenant, &c.note_id)
            .await
            .unwrap_or((None, None));

        // Calculer les facteurs de scoring.
        // M-1 : recency basée sur anchor_ms (date d'événement) avec fallback created_ms.
        // PARITÉ vault_search (F-17) — trust age_days RESTE sur created_ms (concept distinct :
        // âge d'ingestion pour le decay de provenance, indépendant de la fraîcheur d'événement).
        let anchor_ms_for_recency = anchor_ms_map.get(&c.note_id).copied().unwrap_or(created_ms);
        let recency = recency_factor(anchor_ms_for_recency, now_ms);
        let pagerank = pagerank_factor(in_degree);

        // Age en jours pour le decay trust.
        let age_days = ((now_ms - created_ms).max(0) as f64) / 86_400_000.0;

        // Résoudre trust_params depuis la configuration de scoring de l'app.
        let trust_params = state
            .scoring
            .resolve(trust_opt, provenance_opt.as_deref(), age_days);

        // Score composite pondéré (P0-2 : forme multiplicative, parité bit-pour-bit avec défauts).
        let score = composite_score_weighted(c.rrf_score, recency, pagerank, trust_params, weights);
        scored.push((c, score));
    }

    // Tri décroissant par score, tiebreaker ULID (cache constraint 2 — déterminisme).
    // `partial_cmp` est sûr ici : les scores sont des f64 finies
    // (rrf_score > 0, facteurs bornés — NaN impossible avec les inputs valides).
    sort_candidates_by_score_then_ulid(&mut scored);

    // ── Phase 2A : lazy body fetch, accumulation jusqu'au budget inline ─────────
    //
    // Pour chaque candidat dans l'ordre trié, charger le body uniquement si on n'a
    // pas encore atteint le budget inline. Les notes absentes ou en erreur sont ignorées.
    let mut inline: Vec<Selected> = Vec::new();
    let mut budget_inline_used: u32 = 0;
    // `stub_start` : premier index dans `scored` pour la phase stub.
    // Défaut = scored.len() : si la boucle se termine sans break, tout est inline.
    let mut stub_start: usize = scored.len();

    'phase_inline: for (idx, (c, score)) in scored.iter().enumerate() {
        if budget_inline_used >= budget {
            // Budget inline plein — tout le reste part en stubs.
            stub_start = idx;
            break 'phase_inline;
        }

        match state.search.get_note(tenant, &c.note_id).await {
            Ok(Some(record)) => {
                let tokens = estimator.estimate(&record.body_text);

                // Ne pas inclure si l'ajout de cette note ferait dépasser le budget inline.
                // Cette note et les suivantes partent en stubs (elles peuvent être plus petites,
                // mais on respecte l'ordre de tri stable — pas de greedy knapsack).
                if budget_inline_used.saturating_add(tokens) > budget {
                    stub_start = idx;
                    break 'phase_inline;
                }

                // P2-2 : date ISO 8601 UTC depuis epoch ms.
                let date = DateTime::<Utc>::from_timestamp_millis(record.created)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());

                let title = record.title.unwrap_or_else(|| record.id.clone());
                let note_id = record.id;
                inline.push(Selected {
                    note_id,
                    title,
                    section: record.section,
                    date,
                    score: *score,
                    body: record.body_text,
                });
                budget_inline_used = budget_inline_used.saturating_add(tokens);
            }
            Ok(None) => {
                tracing::debug!(
                    note_id = %c.note_id,
                    "select_budget_aware: note absente (inline), ignorée"
                );
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    note_id = %c.note_id,
                    "select_budget_aware: get_note failed (inline), ignorée"
                );
            }
        }
    }

    // ── Phase 2B : stubs pour les candidats hors budget inline ──────────────────
    //
    // Pour chaque candidat restant (à partir de `stub_start`), charger le body
    // uniquement pour extraire le snippet (120 codepoints max). Le body complet
    // n'est PAS compté dans le budget inline — seul le coût estimé du stub rendu
    // est accumulé dans `budget_stub_used`.
    //
    // Les candidats dont le stub dépasse le stub_budget restant sont droppés.
    // On ne break pas immédiatement sur dépassement : un stub trop grand
    // peut être suivi d'un plus petit (pas de tri par taille des stubs).
    let mut stubs: Vec<Stub> = Vec::new();
    let mut budget_stub_used: u32 = 0;

    for (c, score) in &scored[stub_start..] {
        if budget_stub_used >= stub_budget {
            break;
        }

        match state.search.get_note(tenant, &c.note_id).await {
            Ok(Some(record)) => {
                let date = DateTime::<Utc>::from_timestamp_millis(record.created)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());

                // Construire un Selected temporaire pour `stub_from_selected` (snippet).
                // Seul le snippet est utilisé — le body complet n'est pas compté inline.
                let title = record.title.unwrap_or_else(|| record.id.clone());
                let note_id = record.id;
                let sel_for_stub = Selected {
                    note_id,
                    title,
                    section: record.section,
                    date,
                    score: *score,
                    body: record.body_text,
                };

                let stub = stub_from_selected(&sel_for_stub, STUB_SNIPPET_CHARS);
                let stub_tokens = estimator.estimate(&render_stub(&stub));

                if budget_stub_used.saturating_add(stub_tokens) <= stub_budget {
                    budget_stub_used = budget_stub_used.saturating_add(stub_tokens);
                    stubs.push(stub);
                }
                // Sinon : drop — on continue vers les candidats suivants dans l'ordre ULID-stable.
            }
            Ok(None) => {
                tracing::debug!(
                    note_id = %c.note_id,
                    "select_budget_aware: note absente (stub), ignorée"
                );
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    note_id = %c.note_id,
                    "select_budget_aware: get_note failed (stub), ignorée"
                );
            }
        }
    }

    Ok((inline, stubs, budget_inline_used))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::retrieval::Candidate;

    fn make_candidate(note_id: &str) -> Candidate {
        Candidate {
            note_id: note_id.to_string(),
            rrf_score: 1.0,
        }
    }

    /// Tiebreaker ULID : scores ex-aequo → ordre ULID croissant, déterministe sur 2 runs.
    ///
    /// Vérifie que `sort_candidates_by_score_then_ulid` produit un ordre stable et
    /// reproductible quand plusieurs candidats partagent le même score.
    ///
    /// Vérifie aussi que le score prime sur le ULID quand les scores diffèrent.
    #[test]
    fn select_tiebreaker_ulid_stable() {
        // ULIDs avec ordre lexicographique croissant : AA < MM < ZZ.
        let c_low = make_candidate("01JX0AAAAAAAAAAAAAAAAAAAAA"); // ULID bas
        let c_mid = make_candidate("01JX5MMMMMMMMMMMMMMMMMMMMM"); // ULID moyen
        let c_top = make_candidate("01JXZZZZZZZZZZZZZZZZZZZZZZ"); // ULID haut

        // Run 1 : ordre initial inversé.
        let mut scored = vec![
            (c_top.clone(), 1.0_f64),
            (c_low.clone(), 1.0_f64),
            (c_mid.clone(), 1.0_f64),
        ];
        sort_candidates_by_score_then_ulid(&mut scored);
        let order1: Vec<String> = scored.iter().map(|(c, _)| c.note_id.clone()).collect();

        // Run 2 : ordre initial différent.
        let mut scored2 = vec![
            (c_mid.clone(), 1.0_f64),
            (c_top.clone(), 1.0_f64),
            (c_low.clone(), 1.0_f64),
        ];
        sort_candidates_by_score_then_ulid(&mut scored2);
        let order2: Vec<String> = scored2.iter().map(|(c, _)| c.note_id.clone()).collect();

        // Ordre ULID croissant (tiebreaker a.note_id.cmp(&b.note_id)).
        assert_eq!(
            order1[0], c_low.note_id,
            "1er : ULID lexicographiquement le plus bas"
        );
        assert_eq!(order1[1], c_mid.note_id, "2ème : ULID moyen");
        assert_eq!(order1[2], c_top.note_id, "3ème : ULID le plus haut");

        // Déterminisme : 2 runs avec ordres initiaux différents → même ordre final.
        assert_eq!(
            order1, order2,
            "tiebreaker ULID doit être déterministe (indépendant de l'ordre initial)"
        );

        // Le score prime sur le ULID quand ils diffèrent.
        let c_high_score = make_candidate("01JX0BBBBBBBBBBBBBBBBBBBBB"); // ULID bas, score élevé
        let mut scored3 = vec![
            (c_low.clone(), 0.5_f64),        // ULID bas, score faible
            (c_high_score.clone(), 2.0_f64), // ULID bas aussi, mais score élevé
        ];
        sort_candidates_by_score_then_ulid(&mut scored3);
        assert_eq!(
            scored3[0].0.note_id, c_high_score.note_id,
            "score élevé (2.0) prime sur ULID — doit être en 1er"
        );
        assert_eq!(
            scored3[1].0.note_id, c_low.note_id,
            "score faible (0.5) — doit être en 2ème"
        );
    }
}
