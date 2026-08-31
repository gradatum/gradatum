//! Retrieval — récupération des candidats pour l'assemblage de contexte LLM.
//!
//! Ce module implémente la couche de récupération du pipeline `vault_context` v0.7.0 :
//! fusion BM25 + sémantique via `rrf_fuse_short_circuit` (F-162 critère 6 + critère 10) —
//! à bras unique, le score normalisé du bras qui répond fait foi ; à deux bras, la fusion
//! pondérée sur scores normalisés remplace le RRF pur (la magnitude cesse d'être jetée).
//! Dégradation gracieuse (BM25-only) en cas d'échec ou timeout de l'embed.
//!
//! ## Flux principal
//!
//! ```text
//! query ──► ULID? ──yes──► UlidDirect (note + backlinks)
//!               │
//!               no
//!               │
//!           build_fts_query (P1-1 sanitization)
//!               │
//!           vide? ──yes──► candidates: vec![]
//!               │
//!           BM25 (search_fts_with_snippet, limit=top_n*2)
//!               │
//!           embed (tokio::time::timeout, P2-3)
//!               │
//!        ┌─── ok ───► search_semantic ──► sem_hits
//!        │                │
//!        │             erreur/timeout ──► embed_fallback=true, sem=[]
//!        │
//!        └─── Noop ──► embed_fallback=true, sem=[]
//!               │
//!           rrf_fuse_short_circuit(k=60, limit=top_n)
//!               │
//!           RetrievalOutcome { candidates, query_embedding, embed_fallback, kind }
//! ```
//!
//! ## Conformité spec v0.7.0
//!
//! | Spec | Implémentation |
//! |---|---|
//! | P1-1 sanitization FTS5 | `build_fts_query` avant tout appel FTS |
//! | P1-4 ULID-direct | branche `Ulid::from_string` en tête |
//! | P2-1 Candidate sans champs morts | `note_id` + `rrf_score` uniquement |
//! | P2-3 timeout embed | `tokio::time::timeout(embed_timeout_ms)` |
//! | Embedding reuse | `query_embedding` returned in outcome |

use gradatum_core::error::GradatumError;
use gradatum_core::scope::AclCheckedVaultId;
use gradatum_embed::EmbedBackend;
use ulid::Ulid;

use crate::api_v1::handlers::build_fts_query;
use crate::state::AppState;

/// Candidat retourné par [`retrieve_candidates`].
///
/// Does not carry `section`/`title` (resolved in the assembly step
/// via `get_note` only for notes retained within the budget — no dead fields).
#[derive(Debug, Clone)]
pub struct Candidate {
    /// ULID de la note candidate (format string).
    pub note_id: String,
    /// Score RRF composite (ou `1.0` en mode ULID-direct).
    ///
    /// Consumed by [`crate::context::select::select_budget_aware`] for composite score computation.
    pub rrf_score: f64,
}

/// Stratégie de retrieval utilisée — distingue RRF multi-signal d'une lookup par ULID.
///
/// Exposed in [`RetrievalOutcome`] so the assembler can adapt its rendering strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalKind {
    /// Fusion BM25 + sémantique (chemin standard).
    Rrf,
    /// Lookup directe : `query` est un ULID valide → note + backlinks.
    ///
    /// Parité exacte avec le chemin legacy `logic.rs:1010`.
    UlidDirect,
}

/// Full retrieval result — consumed by the assembler.
#[derive(Debug)]
pub struct RetrievalOutcome {
    /// Notes candidates triées par score RRF décroissant, cappées à `top_n`.
    ///
    /// Vide si la requête est vide après sanitization FTS5 (P1-1).
    pub candidates: Vec<Candidate>,
    /// Embedding de la requête calculé durant le retrieval.
    ///
    /// `None` si : mode ULID-direct, embed a échoué, timeout, ou embedder Noop.
    /// Réutilisable pour éviter un second appel embed en aval (F-58).
    ///
    /// Not consumed in all assembly modes. Read by the assembler when skill injection is enabled.
    #[allow(dead_code)]
    pub query_embedding: Option<Vec<f32>>,
    /// `true` si le chemin sémantique a été désactivé (Noop, timeout, erreur).
    ///
    /// `false` si l'embed a réussi — même si `search_semantic` n'a retourné aucun
    /// résultat (pas d'embeddings stockés). Valeur canonique pour `diagnostics.embed_fallback`.
    pub embed_fallback: bool,
    /// Stratégie de retrieval utilisée.
    ///
    /// Not consumed in all assembly modes. The assembler adapts its rendering based on this field.
    #[allow(dead_code)]
    pub kind: RetrievalKind,
}

/// Récupère les candidats pertinents pour une requête dans un vault donné.
///
/// Implements the (hybrid) retrieval pipeline for the LLM context (since v0.7.0).
///
/// ## Algorithme (voir module-level doc pour le diagramme complet)
///
/// 1. **ULID-direct (P1-4)** : si `query` est un ULID valide → `[ulid] ++ backlinks`,
///    `kind=UlidDirect`, sans fusion ni embed.
/// 2. **Sanitization FTS5 (P1-1)** : `build_fts_query(query)`. Vide → early return,
///    aucun appel FTS (évite les erreurs `parse error` FTS5 → pas de 500).
/// 3. **BM25** : `search_fts_with_snippet(limit=top_n*2, section=None)` suivi d'un
///    filtre en mémoire si `sections=Some(set)` (C1 : invariant parité FTS).
/// 4. **Sémantique (P2-3)** : embed borné par `embed_timeout_ms`. Timeout/erreur/Noop →
///    `embed_fallback=true`, fusion dégradée en BM25-only. Pas de panique, pas de 500.
/// 5. **Fusion** : `rrf_fuse_short_circuit(k=60, limit=top_n)` — bras unique → score
///    normalisé du bras ; deux bras → fusion pondérée sur scores normalisés (F-162).
///
/// ## Multi-section filter (since v0.7.1)
///
/// - `sections=None` : aucun filtre — parité exacte avec le comportement pré-v0.7.1.
/// - `sections=Some(set)` : filtre **en mémoire** sur les hits retournés (BM25 via
///   `SearchHitRaw.section`, sémantique via `filter_semantic_by_sections`).
///   **NE PAS ajouter de clause SQL `IN`** dans `build_fts_where_parts` (partagée avec
///   `count_fts_matches`, invariant R3 : 5 prédicats jamais désynchronisés).
///
/// # Errors
///
/// Retourne `GradatumError` uniquement sur échec SQL non récupérable de
/// `search_fts_with_snippet`. Les erreurs embed et `search_semantic` sont absorbées
/// (dégradation gracieuse → `embed_fallback=true`).
pub async fn retrieve_candidates(
    state: &AppState,
    vault_id: &AclCheckedVaultId,
    query: &str,
    sections: Option<&[&str]>,
    top_n: usize,
    embed_timeout_ms: u64,
) -> Result<RetrievalOutcome, GradatumError> {
    // ── P1-4 : ULID-direct (parité legacy logic.rs:1010) ──────────────────────
    //
    // Si la requête est un ULID valide (avec trim pour robustesse), on retourne
    // directement la note + ses backlinks, sans RRF ni embed.
    if let Ok(ulid) = Ulid::from_string(query.trim()) {
        let ulid_str = ulid.to_string();
        let backlinks = state
            .search
            .backlinks(vault_id.as_str(), &ulid_str)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    err = %e,
                    note_id = %ulid_str,
                    "retrieve_candidates: backlinks failed, ULID-direct without backlinks"
                );
                vec![]
            });
        let mut ids = vec![ulid_str];
        ids.extend(backlinks);
        let candidates = ids
            .into_iter()
            .map(|id| Candidate {
                note_id: id,
                rrf_score: 1.0,
            })
            .collect();
        return Ok(RetrievalOutcome {
            candidates,
            query_embedding: None,
            embed_fallback: false,
            kind: RetrievalKind::UlidDirect,
        });
    }

    // ── P1-1 : sanitization FTS5 + guard vide ─────────────────────────────────
    //
    // `build_fts_query` échappe les caractères spéciaux FTS5 et les mots-clés réservés.
    // Un résultat vide après trim indique une requête sans contenu exploitable
    // (espaces, ponctuation pure) → early return sans appel FTS (pas de 500).
    let fts_q = build_fts_query(query);
    if fts_q.trim_matches(['"', ' ']).is_empty() {
        return Ok(RetrievalOutcome {
            candidates: vec![],
            query_embedding: None,
            embed_fallback: false,
            kind: RetrievalKind::Rrf,
        });
    }

    // ── BM25 via FTS5 ──────────────────────────────────────────────────────────
    //
    // `limit = top_n * 2` : buffer RRF — la fusion peut re-classer et réduire.
    // `include_downgraded = false` : exclure les notes archivées.
    // `section = None` : toujours None ici (C1 invariant parité).
    //   Le filtre multi-sections se fait EN MÉMOIRE via `SearchHitRaw.section` ci-dessous,
    //   pour ne pas toucher `build_fts_where_parts` (partagée avec `count_fts_matches`,
    //   invariant R3 : 5 prédicats jamais désynchronisés).
    let bm25_hits = state
        .search
        .search_fts_with_snippet(
            vault_id,
            &fts_q,
            top_n * 2,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;

    // Filtre BM25 en mémoire (C1 — v0.7.1 Task 1).
    //
    // `SearchHitRaw.section` est disponible directement → filtre sans appel SQL
    // supplémentaire. Ne s'applique que si `sections` est `Some(set)`.
    let bm25_hits: Vec<_> = match sections {
        None => bm25_hits,
        Some(set) => bm25_hits
            .into_iter()
            .filter(|h| set.contains(&h.section.as_str()))
            .collect(),
    };

    let bm25_for_rrf: Vec<(String, f64)> = bm25_hits
        .iter()
        .map(|h| (h.note_id.to_string(), h.bm25))
        .collect();

    // ── Sémantique + timeout (P2-3) ────────────────────────────────────────────
    //
    // Noop → dégradation immédiate (embed_fallback=true, sem=[]).
    // Non-Noop → embed borné par `embed_timeout_ms`, puis `search_semantic`.
    // Tout échec (timeout, erreur embed, erreur search_semantic) → dégradation gracieuse.
    let mut embed_fallback = false;
    let mut query_embedding: Option<Vec<f32>> = None;
    let sem_for_rrf: Vec<(String, f32)> = if state.embedder.backend_kind() != EmbedBackend::Noop {
        let timeout_dur = std::time::Duration::from_millis(embed_timeout_ms);
        match tokio::time::timeout(timeout_dur, state.embedder.embed(query)).await {
            Ok(Ok(emb)) => {
                // Embed réussi → tenter search_semantic.
                let sem_result = state
                    .search
                    .search_semantic(
                        vault_id,
                        state.embedder.embedder_id(),
                        &emb,
                        top_n * 2,
                        None,
                    )
                    .await;
                match sem_result {
                    Ok(hits) => {
                        // Filtre sections sémantique (C1 fix v0.7.0, généralisé v0.7.1 Task 1).
                        //
                        // Sans ce filtre, `search_semantic` remonte des notes de toutes
                        // sections alors que le canal BM25 est filtré en mémoire.
                        // Asymétrie → leak de notes hors-section via RRF.
                        // Comportement sur erreur SQL : dégradation BM25-only (hits vides),
                        // aligné sur `filter_semantic_by_sections` (Err → vec![]).
                        let hits = if let Some(wanted_secs) = sections {
                            if hits.is_empty() {
                                hits
                            } else {
                                let ids: Vec<String> =
                                    hits.iter().map(|(id, _)| id.to_string()).collect();
                                let sec_result =
                                    state.search.get_titles_sections(vault_id, &ids).await;
                                crate::api_v1::handlers::filter_semantic_by_sections(
                                    hits,
                                    wanted_secs,
                                    sec_result,
                                )
                            }
                        } else {
                            hits
                        };
                        // Stocker l'embedding pour réutilisation F-58.
                        query_embedding = Some(emb);
                        hits.into_iter()
                            .map(|(id, score)| (id.to_string(), score))
                            .collect()
                    }
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            query = %query,
                            "retrieve_candidates: search_semantic failed, BM25-only fallback"
                        );
                        embed_fallback = true;
                        vec![]
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    err = %e,
                    query = %query,
                    "retrieve_candidates: embed() failed, BM25-only fallback"
                );
                embed_fallback = true;
                vec![]
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_ms = embed_timeout_ms,
                    query = %query,
                    "retrieve_candidates: embed() timed out after {}ms, BM25-only fallback",
                    embed_timeout_ms
                );
                embed_fallback = true;
                vec![]
            }
        }
    } else {
        // Noop embedder → dégradation immédiate.
        embed_fallback = true;
        vec![]
    };

    // ── Fusion (k=60) + cap top_n ─────────────────────────────────────────────
    //
    // `rrf_fuse_short_circuit` (F-162 critère 6 + critère 10) : à bras unique, le
    // score normalisé du bras qui répond fait foi ; à deux bras, la fusion pondérée
    // sur scores normalisés remplace le RRF pur — la magnitude cesse d'être jetée.
    // La décision opérateur 2026-08-24 étend ce reweighting au chemin de contexte
    // (pas seulement `vault_search`), pour l'embedder ACTIF.
    //
    // Le chemin BM25-only par configuration (embedder Noop) reste, lui, sur la fusion
    // par rang pure `rrf_fuse` — rétrocompat bit-à-bit, à l'identique de la garde de
    // `logic.rs` (`vault_search`, tests snapshot `salience_off` inchangés) : sous Noop,
    // `sem_for_rrf` est toujours vide, le court-circuit rendrait `normalize_bm25(...)`
    // (une magnitude sur l'échelle BM25) là où le contrat documenté exige le score de
    // rang `1/(k+rank)`. Ce reweighting mal appliqué déplacerait le classement composite
    // en aval (`select.rs`, `rrf_score` entrée pondérée de `composite_score_weighted`).
    // `k=60` conservé pour compatibilité de signature ; sans effet dans les cas
    // court-circuit/pondéré.
    let fused = if state.embedder.backend_kind() != EmbedBackend::Noop {
        gradatum_search::rrf_fuse_short_circuit(&bm25_for_rrf, &sem_for_rrf, 60.0, top_n)
    } else {
        gradatum_search::rrf_fuse(&bm25_for_rrf, &sem_for_rrf, 60.0, top_n)
    };
    let candidates = fused
        .into_iter()
        .map(|h| Candidate {
            note_id: h.note_id,
            rrf_score: h.rrf_score,
        })
        .collect();

    Ok(RetrievalOutcome {
        candidates,
        query_embedding,
        embed_fallback,
        kind: RetrievalKind::Rrf,
    })
}
