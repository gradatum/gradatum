//! In-process computation of the proactive surface.
//!
//! [`proactive_refresh_tick`] est le point d'entrée flag-gaté (miroir de
//! [`crate::review_promote::promote_tick`]) :
//!
//! - flag `multi_tenant` OFF → délègue à [`proactive_refresh_once`] (tenant `"main"`,
//!   inchangé) ;
//! - flag ON → itère les vaults actifs et rafraîchit CHAQUE surface dans SON propre tenant
//!   via `refresh_vault_surface` (lecture ET écriture scopées, aucun clobber cross-vault).
//!
//! `refresh_vault_surface` calcule la surface mémoire d'un vault :
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

/// Dispatcher flag-gaté du tick proactive-refresh (miroir de
/// [`crate::review_promote::promote_tick`]).
///
/// - `multi_tenant_enabled = false` → délègue à [`proactive_refresh_once`] **inchangé**
///   (tenant `"main"` codé en dur, aucun appel à `list_active_vaults` — comportement
///   inchangé garanti par ce flag-gate, jamais par le contenu de la liste).
/// - `true` → itère les vaults actifs (`list_active_vaults`, triés par id) et rafraîchit la
///   surface de CHAQUE vault dans SON propre tenant (lecture ET écriture scopées). Non-fatal
///   par vault : un échec est loggé et compté, jamais propagé au milieu de la boucle
///   (mêmes garanties non-fatales que `promote_tick`).
///
/// # Retour
///
/// - OFF : le retour de [`proactive_refresh_once`] (taille de la surface `"main"`).
/// - ON : somme des tailles de surface des vaults rafraîchis avec succès.
///
/// # Errors
///
/// - OFF : propage l'erreur de [`proactive_refresh_once`].
/// - ON : `GradatumError` si `list_active_vaults` échoue (tick entier ignoré), ou si au moins
///   un vault a échoué (les autres restent rafraîchis — l'erreur signale l'état dégradé au
///   suivi de tâche, comme `promote_tick` remonte `errors > 0`).
///
/// # Panics
///
/// Jamais.
#[must_use = "the refresh outcome must be recorded for scheduled-task health"]
pub async fn proactive_refresh_tick(
    state: &AppState,
    cfg: &ProactiveRecallConfig,
    multi_tenant_enabled: bool,
) -> Result<usize, GradatumError> {
    if !multi_tenant_enabled {
        return proactive_refresh_once(state, cfg).await;
    }

    let active_vault_ids = state.search.list_active_vaults().await.map_err(|e| {
        tracing::warn!(error = %e, "proactive_refresh: list_active_vaults failed — tick skipped");
        e
    })?;

    let mut total = 0usize;
    let mut errors = 0usize;
    for vault_id in active_vault_ids {
        match refresh_vault_surface(state, cfg, &vault_id).await {
            Ok(n) => total += n,
            Err(e) => {
                tracing::warn!(
                    vault_id = %vault_id,
                    error = %e,
                    "proactive_refresh: vault refresh failed — vault skipped"
                );
                errors += 1;
            }
        }
    }

    if errors > 0 {
        return Err(GradatumError::Storage(format!(
            "proactive_refresh: {errors} vault(s) failed"
        )));
    }
    Ok(total)
}

/// Calcule et persiste la surface mémoire proactive du tenant `"main"`.
///
/// Chemin mono-tenant historique (flag `multi_tenant` OFF) : délègue à
/// `refresh_vault_surface` scopé sur `"main"`.
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
    refresh_vault_surface(state, cfg, &VaultId::new("main")).await
}

/// Calcule et persiste la surface mémoire proactive d'UN vault donné.
///
/// Corps partagé par [`proactive_refresh_once`] (scopé `"main"`) et par la boucle ON de
/// [`proactive_refresh_tick`]. La LECTURE (`list_recent_notes`, `retrieve_candidates`,
/// `get_titles_sections`) comme l'ÉCRITURE (`upsert_surface`) sont scopées sur `vault_id` —
/// aucun croisement de vault, aucun clobber de la surface d'un autre tenant.
///
/// # Errors
///
/// Voir [`proactive_refresh_once`] — mêmes échecs SQL irrécupérables, propagés par vault.
///
/// # Panics
///
/// Jamais.
async fn refresh_vault_surface(
    state: &AppState,
    cfg: &ProactiveRecallConfig,
    vault_id: &VaultId,
) -> Result<usize, GradatumError> {
    // Télémétrie Task 12 — chronomètre global (observé à chaque sortie Ok).
    let start = Instant::now();

    // Job système (tick proactive-refresh) : hors requête HTTP — témoin système, scopé au
    // vault courant (à flag OFF : toujours `"main"`).
    let acl_vault_id = gradatum_core::scope::AclCheckedVaultId::for_system_task(vault_id.clone());
    let now_ms = chrono::Utc::now().timestamp_millis();

    // ── Étape 1 : notes récentes → requête de salience ────────────────────────

    let recent = state
        .search
        .list_recent_notes(acl_vault_id.as_str(), cfg.recent_k)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "proactive_refresh: list_recent_notes failed");
            e
        })?;

    if recent.is_empty() {
        tracing::debug!("proactive_refresh: empty corpus — surface cleared");
        upsert_or_warn(state, vault_id.as_str(), &[], now_ms).await?;
        observe_refresh_done(state, start.elapsed().as_secs_f64());
        return Ok(0);
    }

    let (query, source_ulids) = derive_salience_query(&recent);
    let source_set: HashSet<&str> = source_ulids.iter().map(String::as_str).collect();

    if query.is_empty() {
        tracing::debug!("proactive_refresh: empty salience query — surface cleared");
        upsert_or_warn(state, vault_id.as_str(), &[], now_ms).await?;
        observe_refresh_done(state, start.elapsed().as_secs_f64());
        return Ok(0);
    }

    // ── Étape 2 : retrieval RRF cross-sections ────────────────────────────────
    //
    // Embed KO (Noop ou timeout) → embed_fallback=true → BM25-only, pas d'erreur.
    // L'erreur SQL FTS (seul cas fatal) est propagée via `?`.

    let outcome = retrieve_candidates(
        state,
        &acl_vault_id,
        &query,
        Some(PROACTIVE_SECTIONS),
        cfg.surface_size,
        state.context.embed_timeout_ms,
    )
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "proactive_refresh: retrieve_candidates failed");
        e
    })?;

    if outcome.embed_fallback {
        tracing::debug!("proactive_refresh: embed failed — BM25-only degradation (surface kept)");
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
            "proactive_refresh: all candidates excluded (sources) — surface cleared"
        );
        upsert_or_warn(state, vault_id.as_str(), &[], now_ms).await?;
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
        .get_titles_sections(&acl_vault_id, &candidate_ids)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "proactive_refresh: get_titles_sections failed");
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
    upsert_or_warn(state, vault_id.as_str(), &surface, now_ms).await?;

    tracing::info!(
        surface_len,
        recent_notes = recent.len(),
        candidates_raw = outcome.candidates.len(),
        candidates_after_filter = filtered_candidates.len(),
        embed_fallback = outcome.embed_fallback,
        "proactive_refresh: surface computed"
    );

    observe_refresh_done(state, start.elapsed().as_secs_f64());
    Ok(surface_len)
}

/// Observes the duration of a successful refresh and increments the counter.
///
/// Appelé à chaque sortie `Ok(…)` de [`refresh_vault_surface`] — y compris les
/// retours précoces (corpus vide, salience vide, candidats tous exclus).
/// Les sorties `Err` propagent sans observation (échec SQL non-récupérable).
///
/// `elapsed` est exprimé en secondes (appeler `.as_secs_f64()` sur `Instant::elapsed()`).
#[inline]
fn observe_refresh_done(state: &AppState, elapsed: f64) {
    state.metrics.proactive_refresh_duration.observe(elapsed);
    state.metrics.proactive_refresh.inc();
}

/// UPSERT la surface dans `state.proactive_surface` pour le tenant `tenant`, si présent.
///
/// Skip propre (warn + `Ok(())`) si le store est absent (`None`) — pas de panique.
/// En cas d'erreur SQL, convertit en `GradatumError::Storage` et propage.
///
/// `tenant` est le vault CIBLE : à flag OFF toujours `"main"`, à flag ON le vault courant de
/// la boucle. Router ce paramètre est ce qui empêche la surface d'un vault d'écraser celle
/// d'un autre (clobber `"main"`).
async fn upsert_or_warn(
    state: &AppState,
    tenant: &str,
    surface: &[ProactiveHit],
    now_ms: i64,
) -> Result<(), GradatumError> {
    let Some(store) = state.proactive_surface.as_ref() else {
        tracing::warn!("proactive_refresh: proactive_surface store absent (None) — upsert skipped");
        return Ok(());
    };
    store
        .upsert_surface(tenant, surface, now_ms)
        .await
        .map_err(|e| GradatumError::Storage(format!("proactive_refresh upsert_surface: {e}")))
}
