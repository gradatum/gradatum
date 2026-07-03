//! In-process computation of the proactive surface.
//!
//! [`proactive_refresh_once`] calcule la surface mémoire du tenant `"main"` :
//!
//! 1. Récupère les K notes les plus récentes (`list_recent_notes`).
//! 2. Construit une requête de salience à partir de leurs titres et tags (`derive_salience_query`).
//! 3. Lance un retrieval RRF cross-sections restreint à `lessons-learned`, `reasoning`, `decisions`.
//! 4. Exclut les ULIDs sources (notes récentes déjà actives — ne pas re-surfacer).
//! 5. Hydrate titre et section par batch (`get_titles_sections`, anti-N+1).
//! 6. Persiste la surface dans `ProactiveSurfaceStore` (UPSERT latest-per-tenant).
//!
//! ## Gabarit
//!
//! Pattern exact de `review_promote::promote_once` : `async fn(&AppState, &Cfg)`,
//! accès direct aux stores via `AppState`, aucune file, aucun `Job`, aucun worker.
//!
//! ## Dégradation gracieuse
//!
//! | Condition | Comportement |
//! |---|---|
//! | Corpus vide (aucune note récente) | Surface vide persistée, `Ok(0)` |
//! | Requête salience vide | Surface vide persistée, `Ok(0)` |
//! | Embed KO / Noop | Candidats BM25-only retenus, pas d'erreur |
//! | Tous candidats exclus (sources) | Surface vide persistée, `Ok(0)` |
//! | `state.proactive_surface = None` | Warn + `Ok(0)` (skip propre, pas de panique) |
//! | SQL error (`list_recent_notes`, FTS, `get_titles_sections`, `upsert_surface`) | `Err(GradatumError)` — the interval task logs and skips |

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use gradatum_core::error::GradatumError;
use gradatum_core::scope::VaultId;
use gradatum_dto::ProactiveHit;

use crate::context::retrieval::retrieve_candidates;
use crate::proactive_recall::ProactiveRecallConfig;
use crate::proactive_recall::salience::derive_salience_query;
use crate::state::AppState;

/// Default target sections for proactive retrieval.
///
/// La surface se restreint aux sections à haute valeur mémorielle : leçons,
/// raisonnements, décisions. Autres sections = hors périmètre pour v0.7.1.
const PROACTIVE_SECTIONS: &[&str] = &["lessons-learned", "reasoning", "decisions"];

/// Calcule et persiste la surface mémoire proactive du tenant `"main"`.
///
/// Cycle complet : salience → retrieval RRF → exclusion sources → hydratation → UPSERT.
/// Retourne la longueur de la surface persistée (`0` si corpus vide ou tous candidats exclus).
///
/// # Non-fatal
///
/// Les erreurs embed et sémantiques sont absorbées en interne par `retrieve_candidates`
/// (`embed_fallback=true` → BM25-only). Le manque de `proactive_surface` store est géré
/// with a warning and skips (returns `Ok(0)`, does not interrupt the interval task).
///
/// # Errors
///
/// Retourne `GradatumError` sur échec SQL irrécupérable :
/// - `list_recent_notes` — erreur de lecture de l'index.
/// - `retrieve_candidates` — erreur FTS non récupérable.
/// - `get_titles_sections` — erreur de lecture de l'index.
/// - `upsert_surface` — erreur d'écriture du store surface.
///
/// The calling interval task logs and skips on `Err` — never fatal.
///
/// # Panics
///
/// Jamais.
pub async fn proactive_refresh_once(
    state: &AppState,
    cfg: &ProactiveRecallConfig,
) -> Result<usize, GradatumError> {
    // Télémétrie Task 12 — chronomètre global (observé à chaque sortie Ok).
    let start = Instant::now();

    let vault_id = VaultId::new("main");
    let now_ms = chrono::Utc::now().timestamp_millis();

    // ── Étape 1 : notes récentes → requête de salience ────────────────────────

    let recent = state
        .search
        .list_recent_notes(vault_id.as_str(), cfg.recent_k)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "proactive_refresh: list_recent_notes échoué");
            e
        })?;

    if recent.is_empty() {
        tracing::debug!("proactive_refresh: corpus vide — surface vidée");
        upsert_or_warn(state, &[], now_ms).await?;
        observe_refresh_done(state, start.elapsed().as_secs_f64());
        return Ok(0);
    }

    let (query, source_ulids) = derive_salience_query(&recent);
    let source_set: HashSet<&str> = source_ulids.iter().map(String::as_str).collect();

    if query.is_empty() {
        tracing::debug!("proactive_refresh: requête de salience vide — surface vidée");
        upsert_or_warn(state, &[], now_ms).await?;
        observe_refresh_done(state, start.elapsed().as_secs_f64());
        return Ok(0);
    }

    // ── Étape 2 : retrieval RRF cross-sections ────────────────────────────────
    //
    // Embed KO (Noop ou timeout) → embed_fallback=true → BM25-only, pas d'erreur.
    // L'erreur SQL FTS (seul cas fatal) est propagée via `?`.

    let outcome = retrieve_candidates(
        state,
        &vault_id,
        &query,
        Some(PROACTIVE_SECTIONS),
        cfg.surface_size,
        state.context.embed_timeout_ms,
    )
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "proactive_refresh: retrieve_candidates échoué");
        e
    })?;

    if outcome.embed_fallback {
        tracing::debug!("proactive_refresh: embed KO — dégradation BM25-only (surface conservée)");
    }

    // ── Étape 3 : exclusion des notes sources (déjà actives récemment) ────────
    //
    // Les notes sources sont celles retournées par `list_recent_notes` : elles ont
    // été consultées/éditées récemment et ne doivent pas être re-surfacées.

    let filtered_candidates: Vec<_> = outcome
        .candidates
        .iter()
        .filter(|c| !source_set.contains(c.note_id.as_str()))
        .collect();

    if filtered_candidates.is_empty() {
        tracing::debug!(
            candidates_raw = outcome.candidates.len(),
            "proactive_refresh: tous les candidats exclus (sources) — surface vidée"
        );
        upsert_or_warn(state, &[], now_ms).await?;
        observe_refresh_done(state, start.elapsed().as_secs_f64());
        return Ok(0);
    }

    // Scores indexés par note_id pour la construction des hits après hydratation.
    let score_by_id: HashMap<&str, f64> = filtered_candidates
        .iter()
        .map(|c| (c.note_id.as_str(), c.rrf_score))
        .collect();

    let candidate_ids: Vec<String> = filtered_candidates
        .iter()
        .map(|c| c.note_id.clone())
        .collect();

    // ── Étape 4 : hydratation titre + section (batch anti-N+1) ───────────────

    let meta = state
        .search
        .get_titles_sections(vault_id.as_str(), &candidate_ids)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "proactive_refresh: get_titles_sections échoué");
            e
        })?;

    // ── Étape 5 : construction des ProactiveHit ────────────────────────────────
    //
    // Un candidat absent de `meta` (note supprimée entre retrieval et hydratation)
    // est silencieusement ignoré via `filter_map`.
    // `snippet` = vide en Task 8 — le corps n'est pas chargé ici (Task 10+).

    let surface: Vec<ProactiveHit> = candidate_ids
        .iter()
        .filter_map(|id| {
            let (title_opt, section) = meta.get(id)?;
            let score = score_by_id.get(id.as_str()).copied().unwrap_or(0.0);
            Some(ProactiveHit {
                ulid: id.clone(),
                title: title_opt.clone().unwrap_or_default(),
                section: section.clone(),
                snippet: String::new(),
                score,
            })
        })
        .collect();

    // ── Étape 6 : persistance UPSERT (latest-per-tenant) ─────────────────────

    let surface_len = surface.len();
    upsert_or_warn(state, &surface, now_ms).await?;

    tracing::info!(
        surface_len,
        recent_notes = recent.len(),
        candidates_raw = outcome.candidates.len(),
        candidates_after_filter = filtered_candidates.len(),
        embed_fallback = outcome.embed_fallback,
        "proactive_refresh: surface calculée"
    );

    observe_refresh_done(state, start.elapsed().as_secs_f64());
    Ok(surface_len)
}

/// Observes the duration of a successful refresh and increments the counter.
///
/// Appelé à chaque sortie `Ok(…)` de [`proactive_refresh_once`] — y compris les
/// retours précoces (corpus vide, salience vide, candidats tous exclus).
/// Les sorties `Err` propagent sans observation (échec SQL non-récupérable).
///
/// `elapsed` est exprimé en secondes (appeler `.as_secs_f64()` sur `Instant::elapsed()`).
#[inline]
fn observe_refresh_done(state: &AppState, elapsed: f64) {
    state.metrics.proactive_refresh_duration.observe(elapsed);
    state.metrics.proactive_refresh.inc();
}

/// UPSERT la surface dans `state.proactive_surface` si présent.
///
/// Skip propre (warn + `Ok(())`) si le store est absent (`None`) — pas de panique.
/// En cas d'erreur SQL, convertit en `GradatumError::Storage` et propage.
async fn upsert_or_warn(
    state: &AppState,
    surface: &[ProactiveHit],
    now_ms: i64,
) -> Result<(), GradatumError> {
    let Some(store) = state.proactive_surface.as_ref() else {
        tracing::warn!("proactive_refresh: proactive_surface store absent (None) — upsert skippé");
        return Ok(());
    };
    store
        .upsert_surface("main", surface, now_ms)
        .await
        .map_err(|e| GradatumError::Storage(format!("proactive_refresh upsert_surface: {e}")))
}
