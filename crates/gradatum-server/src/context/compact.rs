//! Compact assembly mode — folded view (since v0.7.2).
//!
//! `assemble_compact` produit une vue foldée destinée à REMPLACER le bloc contexte
//! côté client (1 reset cache). Les notes déjà envoyées dans la session sont
//! re-rankées par pertinence courante : les top-K les plus pertinents restent inline,
//! le reste est fold en stubs (snippet figé depuis `sent_map` pour les notes `sent`).
//!
//! ## Invariants
//!
//! - **`session_id` obligatoire** : compact sans session n'a pas de sens (`InvalidInput`).
//! - **Aucune note sent perdue** : toute note sent apparaît au moins en stub (déréférençable).
//! - **Dedup ULID** : `BTreeMap` (ordre ULID lexicographique croissant, cohérent Tasks 2/3).
//! - **Snippet figé** : les stubs de notes sent portent le snippet du 1er `mark_sent`.
//! - **Pas de `mark_sent`** : compact est un reset, pas un nouveau tour de session.
//!
//! ## Différences avec `assemble_assembled` (Tasks 6/7)
//!
//! | Aspect | `Assembled` | `Compact` |
//! |---|---|---|
//! | Re-promotion sent | INTERDIT (Constraint 4) | AUTORISÉ (reset) |
//! | `mark_sent` | OUI (nouveaux inline) | NON (vue reset) |
//! | `session_id` | Optionnel | Obligatoire |
//! | Invariant sent | stubs seulement | au moins stubs (aucune perte) |
//!
//! ## Fold priority (`fold_score`)
//!
//! Les candidats reçoivent un score compact `1 / (1 + fold_score(size_proxy, age_ms))`
//! avant la sélection via [`select_budget_aware`]. Les notes récentes et petites
//! (bas [`fold_score`]) conservent un score proche de 1.0 → restent inline ;
//! les notes anciennes et grosses (haut [`fold_score`]) tendent vers 0 → démotées en stubs.
//! Proxy de taille : `snippet.len()` capturé au `mark_sent` (borné `STUB_SNIPPET_CHARS`).
//! Notes fraîches (non-sent) : `age_ms = 0`, `size_proxy = 0` → score compact = 1.0.

use std::collections::BTreeMap;
use std::collections::HashMap;

use chrono::Utc;
use gradatum_core::error::GradatumError;
use gradatum_dto::VaultContextRequest;
use gradatum_search::{ScoringWeightsWire, resolve_weights};

use crate::api_v1::dto::{
    ContextCounts, ContextDiagnostics, IncludedNote, StubDto, VaultContextResponse,
};
use crate::context::reference::Stub;
use crate::context::render;
use crate::context::retrieval::{Candidate, retrieve_candidates};
use crate::context::select::select_budget_aware;
use crate::context::tokens::{HeuristicEstimator, TokenEstimator};
use crate::metrics::{AVG_STUB_TOKENS_SAVED, ContextEfficiencyLabel};
use crate::state::AppState;

/// Fold demotion score for Compact mode (since v0.7.2).
///
/// Retourne un score **croissant** avec la taille et l'ancienneté d'une note.
/// Un fold_score élevé indique que la note doit être démotée en stub en priorité ;
/// un fold_score bas indique que la note est récente et/ou petite → conservée inline.
///
/// ## Formule
///
/// ```text
/// fold_score = size_tokens × age_ms.max(0)
/// ```
///
/// - **Déterministe** : pure function, sans randomness ni état global — mêmes entrées → même sortie.
/// - **Borne non-négatif** : `age_ms.max(0)` gère les horloges skewed (`ts_ms > now_ms`).
/// - **Proxy de taille** : `size_tokens` est l'estimation tokens issue du snippet capturé au
///   `mark_sent` (borné `STUB_SNIPPET_CHARS = 120` chars). Pour les notes non-sent,
///   `size_tokens = 0` et `age_ms = 0` → `fold_score = 0.0` (priorité inline maximale).
///
/// ## Overflow
///
/// `f64` supporte jusqu'à ≈ 1.8×10³⁰⁸. Le produit maximal réaliste
/// (`size ≤ 8 000`, `age ≤ 3×10¹²` ms ≈ 95 ans) ≈ 2.4×10¹⁶, largement dans la plage `f64`.
///
/// # Returns
///
/// Score en `f64`, toujours ≥ 0.0.
#[must_use]
pub fn fold_score(size_tokens: u32, age_ms: i64) -> f64 {
    size_tokens as f64 * age_ms.max(0) as f64
}

/// Compact assembly mode — folded view (since v0.7.2).
///
/// Produit une vue foldée destinée à REMPLACER le bloc contexte côté client (1 reset cache).
/// Contrairement à `super::assemble_assembled`, le mode compact RE-PROMEUT les notes `sent`
/// si elles sont parmi les top-K les plus pertinentes maintenant (reset autorisé).
///
/// ## Algorithme
///
/// 1. `session_id` obligatoire — absent ou invalide → [`GradatumError::InvalidInput`].
/// 2. `get_sent(tenant, session_id)` → ensemble des ULIDs déjà envoyés (snippet figé).
/// 3. `retrieve_candidates(req.query)` → hits courants scorés par RRF.
/// 4. Union (`sent` ∪ hits courants) : notes sent absentes du retrieval ajoutées avec
///    `rrf_score = 0.0` (pertinence courante nulle → fold en stubs en priorité).
/// 5. `select_budget_aware` : top-K pertinents inline (corps complet), reste en stubs.
/// 6. Stubs de notes sent → snippet figé depuis `sent_map` (Constraint 5).
/// 7. Notes sent non représentées après sélection → forcées en stubs minimaux (aucune perte).
/// 8. Rendu via `render_assembled` (inline) + `render_references_block` (stubs).
///
/// # Errors
///
/// - [`GradatumError::InvalidInput`] si `session_id` absent ou format ULID invalide.
/// - [`GradatumError`] propagé depuis [`retrieve_candidates`] sur échec SQL non récupérable.
pub async fn assemble_compact(
    state: &AppState,
    tenant: &str,
    req: &VaultContextRequest,
    identity_privileged: bool,
) -> Result<VaultContextResponse, GradatumError> {
    // ── 1. session_id obligatoire ────────────────────────────────────────────
    //
    // Compact sans session n'a pas de sens : la vue foldée opère sur le `sent_map`
    // de la session courante pour choisir ce qui reste inline et ce qui est fold.
    let session_id = req.session_id.as_deref().ok_or_else(|| {
        GradatumError::InvalidInput(
            "mode=compact requires a session_id (compact without a session is meaningless)"
                .to_owned(),
        )
    })?;

    // Validation format ULID (aligné P2-2 / assemble_assembled).
    if !super::is_session_id_valid(session_id) {
        return Err(GradatumError::InvalidInput(
            "invalid session_id: expected ULID of 26 alphanumeric chars (Crockford base32)"
                .to_owned(),
        ));
    }

    // ── 2. Budget et paramètres ──────────────────────────────────────────────
    let budget: u32 = req
        .budget_tokens
        .or(req.max_tokens)
        .unwrap_or(state.context.default_budget_tokens)
        .clamp(1, 8000);
    let vault_id = crate::api_v1::tenant_guard::own_vault_checked(tenant);
    let top_n = state.context.top_n_candidates;
    let now_ms = Utc::now().timestamp_millis();
    let estimator = HeuristicEstimator;

    // ── 3. get_sent — ensemble des notes déjà envoyées dans la session ───────
    //
    // Dégradation gracieuse si store absent (P2-4) : compact fonctionne sans sent_map
    // (produit une vue assembled standard sans enrichissement session).
    // get_sent failure → warn + HashMap vide (non-critique, contexte retourné quand même).
    let session_sent = match &state.session_trace {
        Some(store) => match store.get_sent(tenant, session_id).await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    session_id = %session_id,
                    "assemble_compact: get_sent failed — compact without sent_map (degradation)"
                );
                HashMap::new()
            }
        },
        None => {
            tracing::debug!(
                session_id = %session_id,
                "assemble_compact: session_trace absent — compact without sent_map (P2-4)"
            );
            HashMap::new()
        }
    };

    // ── 4. retrieve_candidates — re-rank de la pertinence courante ───────────
    let single_section_buf;
    let sections: Option<&[&str]> = if let Some(s) = req.section.as_deref() {
        single_section_buf = [s];
        Some(&single_section_buf)
    } else {
        None
    };

    let outcome = retrieve_candidates(
        state,
        &vault_id,
        &req.query,
        sections,
        top_n,
        state.context.embed_timeout_ms,
    )
    .await?;

    let embed_fallback = outcome.embed_fallback;

    // ── 5. Union (sent ∪ hits courants) + scores compacts fold_score (Task 9) ─────────
    //
    // Les notes sent absentes du retrieval courant sont ajoutées à la liste, puis TOUS
    // les candidats (retrieval + sent-only) reçoivent un score compact inversé au
    // fold_score(size_proxy, age_ms) :
    //
    //   rrf_compact = 1.0 / (1.0 + fold_score(size_proxy, age_ms))
    //
    // - Notes fraîches (non-sent, age=0, size=0) → fold=0 → rrf=1.0 (inline prioritaire).
    // - Notes sent récentes (ts_ms proche de now_ms) → fold petit → rrf proche de 1.0.
    // - Notes sent anciennes+grosses → fold grand → rrf proche de 0.0 (stub prioritaire).
    //
    // Proxy taille : `snippet.len()` (borné `STUB_SNIPPET_CHARS` par `mark_sent`) pour
    // les notes sent ; 0 pour les notes non-sent (taille inconnue = priorité inline).
    //
    // ECON: O(sent_map.len() × candidates.len()) pour le filtre sent_only ;
    //        O(candidates.len()) pour la mise à jour des scores. Sessions bornées.
    let mut candidates: Vec<Candidate> = outcome.candidates;

    // Ajouter les notes sent-only (absentes du retrieval courant).
    let sent_only: Vec<String> = session_sent
        .keys()
        .filter(|ulid| !candidates.iter().any(|c| c.note_id == **ulid))
        .cloned()
        .collect();
    for ulid in sent_only {
        candidates.push(Candidate {
            note_id: ulid,
            // Sera écrasé ci-dessous par le score compact fold-inversé.
            rrf_score: 0.0,
        });
    }

    // Remplacer les rrf_scores par les scores compacts basés sur fold_score (Task 9).
    // En mode compact, le critère inline/stub est l'ancienneté×taille, pas le RRF courant.
    for c in &mut candidates {
        let age_ms = session_sent
            .get(&c.note_id)
            .map(|e| now_ms.saturating_sub(e.ts_ms).max(0))
            .unwrap_or(0);
        // Proxy de taille : snippet.len() pour les notes sent (borné STUB_SNIPPET_CHARS) ;
        // 0 pour les non-sent (note fraîche — taille inconnue sans fetch body).
        let size_proxy = session_sent
            .get(&c.note_id)
            .map(|e| e.snippet.len() as u32)
            .unwrap_or(0);
        let fs = fold_score(size_proxy, age_ms);
        // 1/(1+fs) ∈ (0, 1] : 1.0 quand fs=0 (fraîche/petite) ; tend vers 0 pour fs grand.
        c.rrf_score = 1.0 / (1.0 + fs);
    }

    let candidates_considered = candidates.len() as u32;

    if candidates.is_empty() {
        return Ok(VaultContextResponse {
            assembled_text: String::new(),
            included: vec![],
            budget_used: 0,
            diagnostics: ContextDiagnostics {
                candidates_considered: 0,
                included_count: 0,
                embed_fallback,
                skills_injected: 0,
            },
            references: vec![],
            counts: ContextCounts {
                inline: 0,
                stub: 0,
                dropped: 0,
            },
            // budget_used=0 → seuil jamais atteint.
            cache_breakpoint_hint: false,
        });
    }

    // ── 6. Sélection budget-aware ─────────────────────────────────────────────
    //
    // Compact expose toujours les stubs (vue foldée = reference_mode implicitement actif).
    // `stub_budget_tokens` depuis la config (même que `reference_mode=true` dans assembled).
    let wire: Option<ScoringWeightsWire> = req.scoring.as_ref().map(|sw| ScoringWeightsWire {
        recency: sw.recency,
        pagerank: sw.pagerank,
        trust: sw.trust,
    });
    let weights = resolve_weights(wire.as_ref());
    let stub_budget = state.context.stub_budget_tokens;

    let (selected, stubs, _) = select_budget_aware(
        state,
        tenant,
        candidates,
        &weights,
        &estimator,
        budget,
        stub_budget,
        now_ms,
    )
    .await?;

    // ── Guard identity F-34 (parité vault_search_impl) ────────────────────────
    //
    // Exclure les âmes d'agents (`section == "identity"`) pour un caller non-privilégié,
    // AVANT la construction de `inline_ulids`, des stubs, de `included` et du rendu →
    // aucune fuite de corps ni de titre/snippet. Sections issues de `get_note` (non
    // vides sur ce chemin). Les force-stubs de notes `sent` (étape 7b, section vide,
    // titre = ULID) ne sont pas concernés : ils ne réexposent qu'un ULID déjà envoyé au
    // client dans un tour précédent. No-op pour les callers privilégiés.
    let (selected, stubs): (Vec<_>, Vec<_>) = if identity_privileged {
        (selected, stubs)
    } else {
        (
            selected
                .into_iter()
                .filter(|s| !crate::api_v1::logic::identity_section_hidden(false, &s.section))
                .collect(),
            stubs
                .into_iter()
                .filter(|s| !crate::api_v1::logic::identity_section_hidden(false, &s.section))
                .collect(),
        )
    };

    // ── 7. Post-traitement : snippets figés + invariant "aucune note sent perdue" ──
    //
    // (a) Stubs du select_budget_aware : si ULID dans sent_map → snippet figé (Constraint 5).
    // (b) Dedup via BTreeMap (ordre ULID lexicographique, cohérent Tasks 2/3).
    //     Priorité : stubs select > force-stubs sent-dropped.
    // (c) Notes sent absentes de inline+stubs → forcées en stubs minimaux.
    //     Cas rare : stub_budget trop serré pour absorber toutes les notes sent-only (rrf=0).
    //     Titre/section non disponibles (get_note non appelé pour les dropped) → ULID fallback.
    //     Le client peut déréférencer via vault_read pour le titre complet.
    //     ECON: évite un get_note supplémentaire pour des notes hors budget.

    // Ensemble des ULIDs déjà inline (ne doivent pas apparaître en stubs — dedup).
    let inline_ulids: std::collections::HashSet<&str> =
        selected.iter().map(|s| s.note_id.as_str()).collect();

    let mut stub_map: BTreeMap<String, Stub> = BTreeMap::new();

    // Priorité 1 : stubs produits par select_budget_aware (snippet figé si dans sent_map).
    for mut s in stubs {
        if let Some(entry) = session_sent.get(&s.ulid) {
            // Snippet figé du 1er mark_sent (Constraint 5 — cohérence avec Task 7).
            s.snippet = entry.snippet.clone();
        }
        stub_map.entry(s.ulid.clone()).or_insert(s);
    }

    // Priorité 2 : notes sent non représentées (dropped) → stub minimal (aucune perdue).
    // `or_insert` : si déjà en stubs (Priorité 1), on ne remplace pas.
    for (ulid, entry) in &session_sent {
        if !inline_ulids.contains(ulid.as_str()) {
            stub_map.entry(ulid.clone()).or_insert_with(|| Stub {
                ulid: ulid.clone(),
                // Titre inconnu (get_note non appelé pour les dropped) → ULID fallback.
                // Le client déréférence via vault_read pour le titre complet.
                title: ulid.clone(),
                section: String::new(),
                snippet: entry.snippet.clone(),
            });
        }
    }

    let all_stubs: Vec<Stub> = stub_map.into_values().collect();

    // ── 8. Compteurs ─────────────────────────────────────────────────────────
    //
    // dropped = candidats initiaux non représentés (ni inline ni stubs finaux).
    // Après force-stub, tous les sent sont représentés → dropped peut être 0 ou légèrement
    // positif (candidats retrieval hors budget + hors session_sent).
    let inline_count = selected.len();
    let stub_count = all_stubs.len();
    let dropped_count = (candidates_considered as usize).saturating_sub(inline_count + stub_count);

    let references: Vec<StubDto> = all_stubs
        .iter()
        .map(|s| StubDto {
            ulid: s.ulid.clone(),
            title: s.title.clone(),
            section: s.section.clone(),
            snippet: s.snippet.clone(),
        })
        .collect();

    let included: Vec<IncludedNote> = selected
        .iter()
        .map(|s| IncludedNote {
            ulid: s.note_id.clone(),
            title: s.title.clone(),
            section: s.section.clone(),
            date: s.date.clone(),
            score: s.score,
        })
        .collect();

    // ── 9. Rendu ─────────────────────────────────────────────────────────────
    //
    // Pas d'injection skills en mode compact (reset complet, priorité aux notes).
    // Budget mesuré après rendu complet (P2-b : inclut scaffolding Markdown).
    // Bloc References appendé APRÈS l'estimation de budget (non imputé au budget inline).
    let mut assembled_text = render::render_assembled(&req.query, &selected, &[]);
    let budget_used = estimator.estimate(&assembled_text);
    let included_count = included.len() as u32;

    assembled_text.push_str(&render::render_references_block(&all_stubs));

    // ── Task 11 v0.7.2 : métriques context efficiency (F-30) ────────────────
    //
    // Observé sur le chemin nominal uniquement (counts calculés ci-dessus).
    // `context_compaction_total` : +1 par appel compact nominal (vue foldée produite).
    // `context_tokens_saved` : estimation tokens économisés par fold (stub_count × 200).
    // inc_by : valeurs bornées par top_n_candidates (≤ 500) ≪ u64::MAX, cast as u64 sûr.
    {
        let ctx_label = ContextEfficiencyLabel { mode: "compact" };
        state
            .metrics
            .context_inline_total
            .get_or_create(&ctx_label)
            .inc_by(inline_count as u64);
        state
            .metrics
            .context_stub_total
            .get_or_create(&ctx_label)
            .inc_by(stub_count as u64);
        state
            .metrics
            .context_dropped_total
            .get_or_create(&ctx_label)
            .inc_by(dropped_count as u64);
        // +1 par appel compact nominal (vue foldée F-30 produite).
        state.metrics.context_compaction_total.inc();
        // Estimation tokens économisés par fold : stub_count × AVG_STUB_TOKENS_SAVED.
        state
            .metrics
            .context_tokens_saved
            .observe(stub_count as f64 * AVG_STUB_TOKENS_SAVED);
    }

    Ok(VaultContextResponse {
        assembled_text,
        included,
        budget_used,
        diagnostics: ContextDiagnostics {
            candidates_considered,
            included_count,
            embed_fallback,
            skills_injected: 0,
        },
        references,
        counts: ContextCounts {
            inline: inline_count,
            stub: stub_count,
            dropped: dropped_count,
        },
        cache_breakpoint_hint: budget_used > state.context.cache_breakpoint_threshold_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::fold_score;

    /// `fold_prioritizes_big_old` — l'ordre de démotion suit taille × ancienneté.
    ///
    /// 3 notes aux profils distincts :
    /// - A : grosse (1 000 tokens) + ancienne (1 000 000 ms ≈ 17 min) → fold élevé.
    /// - B : petite (10 tokens) + récente (1 000 ms = 1 s) → fold bas.
    /// - C : moyenne (100 tokens) + age moyen (10 000 ms = 10 s) → fold intermédiaire.
    ///
    /// Invariant : fold_score(A) > fold_score(C) > fold_score(B).
    /// Les plus hauts fold_scores sont démotés en premier (gardés en stub).
    #[test]
    fn fold_prioritizes_big_old() {
        let score_a = fold_score(1_000, 1_000_000); // grosse + ancienne
        let score_b = fold_score(10, 1_000); // petite + récente
        let score_c = fold_score(100, 10_000); // moyenne

        assert!(
            score_a > score_c,
            "grosse+ancienne (score_a={score_a}) doit avoir un fold_score \
             plus élevé que la note moyenne (score_c={score_c})"
        );
        assert!(
            score_c > score_b,
            "note moyenne (score_c={score_c}) doit avoir un fold_score \
             plus élevé que petite+récente (score_b={score_b})"
        );
        // Vérification des valeurs attendues pour documenter la formule.
        // fold_score = size_tokens * age_ms : 1000*1_000_000 = 1_000_000_000
        assert_eq!(score_a, 1_000_000_000.0_f64, "A: 1000 × 1_000_000");
        assert_eq!(score_c, 1_000_000.0_f64, "C: 100 × 10_000");
        assert_eq!(score_b, 10_000.0_f64, "B: 10 × 1_000");
    }

    /// `fold_score_deterministic` — pure function : mêmes entrées → même f64 sur 2 appels.
    ///
    /// Garantit l'absence d'effet de bord (pas de randomness, pas d'état global).
    #[test]
    fn fold_score_deterministic() {
        let size: u32 = 512;
        let age: i64 = 3_600_000; // 1 heure en ms

        let result1 = fold_score(size, age);
        let result2 = fold_score(size, age);

        assert_eq!(
            result1, result2,
            "fold_score doit être déterministe : mêmes entrées → même f64"
        );
        // Valeur attendue : 512 × 3_600_000 = 1_843_200_000
        assert_eq!(
            result1, 1_843_200_000.0_f64,
            "512 × 3_600_000 = 1_843_200_000"
        );
    }

    /// `fold_score_negative_age_is_zero` — age_ms négatif (horloge skewed) → fold = 0.0.
    ///
    /// Si `ts_ms > now_ms` (horloge désynchronisée ou race condition), `age_ms.max(0) = 0`
    /// → fold_score = 0 → note traitée comme fraîche (pas de démotion injuste).
    #[test]
    fn fold_score_negative_age_is_zero() {
        let score = fold_score(1_000, -500_000); // ts_ms dans le futur
        assert_eq!(
            score, 0.0_f64,
            "age_ms négatif (horloge skewed) → fold_score = 0 (note traitée comme fraîche)"
        );
    }

    /// `fold_score_zero_inputs_is_zero` — note fraîche non-sent : fold = 0.0.
    ///
    /// Notes non-sent ont size_proxy=0 et age_ms=0 → fold=0 → rrf_compact=1.0 (inline max).
    #[test]
    fn fold_score_zero_inputs_is_zero() {
        assert_eq!(fold_score(0, 0), 0.0_f64, "note fraîche → fold_score = 0");
        assert_eq!(fold_score(0, 999_999), 0.0_f64, "taille=0 → fold_score = 0");
        assert_eq!(fold_score(999, 0), 0.0_f64, "age=0 → fold_score = 0");
    }
}
