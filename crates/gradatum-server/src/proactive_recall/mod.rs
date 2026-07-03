//! Proactive Recall — module calcul de surface in-process (B').
//!
//! Ce module regroupe la logique de rappel proactif côté serveur :
//! construction de la requête de salience depuis l'activité récente (`salience`),
//! surface computation (`refresh`), and pull orchestration
//! ([`proactive_recall`]).
//!
//! Pattern B' (GO opérateur 2026-06-27) : toutes les fonctions s'exécutent in-process,
//! avec accès direct à `AppState`. Aucun worker, aucune variante `Job`.
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::error::GradatumError;
use gradatum_core::scope::VaultId;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{
    ProactiveHit, ProactiveRecallFeedbackRequest, ProactiveRecallRequest, ProactiveRecallResponse,
};
use ulid::Ulid;

use crate::api_v1::logic::{locus_for_section, locus_for_tenant};
use crate::api_v1::tenant_guard::effective_tenant;
use crate::context::retrieval::retrieve_candidates;
use crate::metrics::ProactiveRecallModeLabel;
use crate::state::AppState;

pub mod refresh;
pub mod salience;

/// Configuration du calcul de surface proactif (F-46, Active Recall).
///
/// Désérialisable depuis le TOML de configuration (section `[proactive_recall]`).
/// Chaque champ possède un défaut raisonnable (OOBE, ADN 4) — la section peut
/// être absente du TOML : les trois défauts s'appliquent alors intégralement.
///
/// ## Plancher 60s
///
/// `refresh_interval_secs` est préservé tel quel lors de la désérialisation.
/// Le plancher de 60s est appliqué dans `main.rs` au moment de construire le
/// `tokio::interval` (`.max(60)`) — une valeur TOML < 60 est remontée à 60s
/// silencieusement. Ce plancher protège contre `interval(Duration::from_secs(0))`
/// qui paniquerait le runtime.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProactiveRecallConfig {
    /// Intervalle (en secondes) entre deux calculs de surface.
    ///
    /// Valeurs < 60 sont remontées à 60s dans `main.rs` (plancher — cf. doc module).
    ///
    /// Valeur par défaut : 900 s (15 minutes).
    #[serde(default = "ProactiveRecallConfig::default_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
    /// Nombre de notes récentes utilisées pour construire la requête de salience.
    ///
    /// Valeur par défaut : 20.
    #[serde(default = "ProactiveRecallConfig::default_recent_k")]
    pub recent_k: usize,
    /// Taille maximale de la surface proactive (nombre de hits retenus).
    ///
    /// Valeur par défaut : 8.
    #[serde(default = "ProactiveRecallConfig::default_surface_size")]
    pub surface_size: usize,
}

impl ProactiveRecallConfig {
    /// Valeur par défaut de `refresh_interval_secs` (source unique, évite la duplication).
    fn default_refresh_interval_secs() -> u64 {
        900
    }

    /// Valeur par défaut de `recent_k`.
    fn default_recent_k() -> usize {
        20
    }

    /// Valeur par défaut de `surface_size`.
    fn default_surface_size() -> usize {
        8
    }
}

impl Default for ProactiveRecallConfig {
    /// Valeurs par défaut raisonnables (OOBE) — aucun service externe requis.
    ///
    /// `refresh_interval_secs = 900` · `recent_k = 20` · `surface_size = 8`.
    ///
    /// Délègue aux fonctions privées pour éviter la duplication des littéraux.
    fn default() -> Self {
        Self {
            refresh_interval_secs: Self::default_refresh_interval_secs(),
            recent_k: Self::default_recent_k(),
            surface_size: Self::default_surface_size(),
        }
    }
}

// ── Orchestrateur pull (Task 10) ─────────────────────────────────────────────
/// Label de mode : surface proactive pré-calculée (lecture du store).
const MODE_PROACTIVE: &str = "proactive";
/// Label de mode : retrieval RRF contextuel à la demande.
const MODE_CONTEXTUAL: &str = "contextual";

/// Nombre d'items retournés par défaut quand la requête omet `limit`
/// (aligné sur la doc DTO `ProactiveRecallRequest::limit`).
const DEFAULT_LIMIT: u32 = 10;
/// Borne haute du nombre d'items retournés (safety cap anti-DoS, ADN 5).
const MAX_LIMIT: u32 = 20;

/// Borne haute du nombre d'`accepted_ulids` dans un feedback (safety cap anti-DoS, ADN 5).
///
/// Par construction du pull, `accepted ⊆ surfaced` et `surfaced` ≤ [`MAX_LIMIT`] (20) :
/// cette borne (large, 64) ne rejette donc JAMAIS une entrée légitime — elle protège
/// uniquement contre un body forgé (parse ULID + allocation `HashSet`) avant tout travail SQL.
const MAX_ACCEPTED_ULIDS: usize = 64;

/// Orchestre un rappel proactif (pull) — lecture de surface ou retrieval contextuel.
///
/// Deux modes, déterminés par la présence de `req.context` :
///
/// - **`context = None`** → mode `"proactive"` : lit la surface pré-calculée
///   ([`crate::proactive_surface_store::ProactiveSurfaceStore::get_surface`]). Surface absente → items vides (pas d'erreur).
/// - **`context = Some(_)`** → mode `"contextual"` : retrieval RRF à la demande
///   ([`retrieve_candidates`]) restreint à `req.sections` (cross-section no-leak).
///
/// ## Sécurité ACL (C3, BLOQUANT)
///
/// The proactive surface is computed by the in-process interval task **without
/// a caller**, over the fixed sections (`lessons-learned`, `reasoning`,
/// `decisions`). Au pull, un appelant dont les `read_patterns` n'autorisent pas une
/// section recevrait des notes cachées = **bypass ACL**. On applique donc, **dans les
/// deux modes**, un re-filtrage par section réutilisant le predicate exact de
/// `vault_context` (`acl.evaluate(Read, {tenant}/{section})`) — voir [`section_readable`].
/// La liste `surfaced` enregistrée en session est la liste **POST-filtrage ACL**
/// (consistency with the `accepted ⊆ surfaced` validation).
///
/// ## Tenant
///
/// Le tenant effectif est dérivé du JWT ([`effective_tenant`]) — le `req.tenant_id`
/// du body est seulement vérifié pour cohérence (divergence → `Forbidden`). Le gate
/// de base ACL porte sur `{tenant}/main`, comme `vault_authors`/`vault_tags`.
///
/// ## Sanitization
///
/// En mode contextuel, le texte `req.context` est passé **brut** à
/// [`retrieve_candidates`], qui le sanitize en interne via `build_fts_query`
/// (étape P1-1) : passer un texte déjà échappé provoquerait un double-échappement FTS.
///
/// # Errors
///
/// - [`GradatumError::Unauthorized`] — appelant non authentifié.
/// - [`GradatumError::Forbidden`] — tenant divergent du JWT, ou ACL Read refusée sur
///   le locus de base `{tenant}/main`.
/// - [`GradatumError::Storage`] — échec SQL irrécupérable (`get_surface`, retrieval,
///   hydratation, `insert_session`).
///
/// # Panics
///
/// Jamais.
pub async fn proactive_recall(
    state: &AppState,
    trust: &TrustContext,
    req: ProactiveRecallRequest,
) -> Result<ProactiveRecallResponse, GradatumError> {
    // Télémétrie Task 12 — chronomètre global (observé à la sortie Ok).
    let start = Instant::now();

    // ── Enforcement ACL/tenant — predicate exact de vault_context ──────────────
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    if state
        .acl
        .evaluate(trust, AclOp::Read, &locus_for_tenant(&tenant))
        != AclDecision::Allow
    {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let now_ms = chrono::Utc::now().timestamp_millis();

    // ── Construction des items selon le mode ──────────────────────────────────
    let (mode, hydrated): (&'static str, Vec<ProactiveHit>) = match req.context.as_deref() {
        None => (MODE_PROACTIVE, read_surface(state, &tenant).await?),
        Some(context) => (
            MODE_CONTEXTUAL,
            contextual_hits(state, &tenant, context, req.sections.as_deref(), limit).await?,
        ),
    };

    // ── C3 (BLOQUANT) : re-filtrage ACL par section + guard identity + cap limit ─
    //
    // Guard identity F-34 (parité vault_search_impl / vault_context_impl) : l'ACL Read
    // sur le locus `{tenant}/identity` peut être Allow (la restriction par-âme n'est PAS
    // encodée dans l'ACL mais dans un guard applicatif dédié). Sans ce filtre, les TITRES
    // des âmes d'agents fuiteraient cross-agent via la surface proactive. Exclusion simple
    // (surface RAG générique, pas de matching par-agent). No-op pour Studio / main-agent.
    let identity_privileged = crate::api_v1::logic::is_identity_privileged(trust);
    let items: Vec<ProactiveHit> = hydrated
        .into_iter()
        .filter(|hit| {
            section_readable(state, trust, &tenant, &hit.section)
                && !crate::api_v1::logic::identity_section_hidden(identity_privileged, &hit.section)
        })
        .take(limit)
        .collect();

    // ── Télémétrie Task 12 : surfaced = nombre d'items POST-filtrage ACL ───────
    let mode_label = ProactiveRecallModeLabel { mode };
    state
        .metrics
        .proactive_surfaced
        .get_or_create(&mode_label)
        .inc_by(items.len() as u64);

    // ── Session : surfaced = items POST-filtrage ACL ──────────────────────────
    let recall_id = Ulid::new().to_string();
    let surfaced: Vec<String> = items.iter().map(|h| h.ulid.clone()).collect();
    if let Some(store) = state.proactive_recall.as_ref() {
        store
            .insert_session(&recall_id, &tenant, mode, &surfaced, now_ms)
            .await
            .map_err(|e| GradatumError::Storage(format!("proactive_recall insert_session: {e}")))?;
    } else {
        tracing::warn!(
            "proactive_recall: proactive_recall store absent (None) — session non persistée"
        );
    }

    // ── Télémétrie Task 12 : durée totale du pull (observée inconditionnellement) ─
    state
        .metrics
        .proactive_recall_duration
        .get_or_create(&mode_label)
        .observe(start.elapsed().as_secs_f64());

    Ok(ProactiveRecallResponse {
        recall_id,
        mode: mode.to_owned(),
        items,
    })
}

// ── Orchestrateur feedback (Task 11) ─────────────────────────────────────────

/// Enregistre le feedback d'acceptation pour une session de rappel proactif.
///
/// Corrèle les notes effectivement acceptées par l'utilisateur (`accepted_ulids`)
/// avec la surface qui lui a été présentée (`surfaced`, enregistrée par
/// [`proactive_recall`]). Idempotent : 2× le même feedback = 1 enregistrement.
///
/// ## Enforcement ACL/tenant (identique à [`proactive_recall`])
///
/// Même barrière exacte que le pull : `is_authenticated` → `effective_tenant`
/// (divergence body/JWT → `Forbidden`) → gate `acl.evaluate(Read, {tenant}/main)`.
/// La surface (`surfaced`) a déjà été filtrée ACL par section au moment du pull
/// therefore validating `accepted ⊆ surfaced` is sufficient to prevent any
/// feedback sur une note que l'appelant n'aurait pas dû voir.
///
/// ## Validation (ordre)
///
/// 1. ACL/tenant (ci-dessus).
/// 2. Safety cap : `accepted_ulids.len()` ≤ [`MAX_ACCEPTED_ULIDS`] — sinon `InvalidInput`
///    (400), AVANT toute lecture SQL (anti-DoS).
/// 3. `recall_id` existe **dans le tenant effectif** ([`crate::proactive_recall_store::ProactiveRecallStore::get_surfaced`])
///    — sinon `InvalidInput` (400). Le filtre tenant interdit le feedback cross-tenant (IDOR).
///    `recall_id` est un identifiant de session, pas un `NoteId` → `InvalidInput`
///    (400) plutôt que `NoteNotFound` (404, typé `NoteId`).
/// 4. Chaque `accepted_ulid` parse en ULID — sinon `InvalidInput` (400).
/// 5. **Invariant de cohérence** `accepted ⊆ surfaced` : tout ULID accepté DOIT
///    figurer dans la surface présentée — sur-ensemble → `InvalidInput` (400).
/// 6. [`crate::proactive_recall_store::ProactiveRecallStore::record_feedback`] (UPSERT idempotent).
///
/// # Errors
///
/// - [`GradatumError::Unauthorized`] — appelant non authentifié.
/// - [`GradatumError::Forbidden`] — tenant divergent du JWT, ou ACL Read refusée sur
///   le locus de base `{tenant}/main`.
/// - [`GradatumError::InvalidInput`] — `accepted_ulids` au-delà du cap, `recall_id` inconnu
///   (ou appartenant à un autre tenant), ULID accepté mal formé, ou `accepted ⊄ surfaced`.
/// - [`GradatumError::Storage`] — échec SQL irrécupérable (`get_surfaced`, `record_feedback`).
///
/// # Panics
///
/// Jamais.
pub async fn proactive_recall_feedback(
    state: &AppState,
    trust: &TrustContext,
    req: ProactiveRecallFeedbackRequest,
) -> Result<(), GradatumError> {
    // ── Enforcement ACL/tenant — barrière identique à `proactive_recall` ───────
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    if state
        .acl
        .evaluate(trust, AclOp::Read, &locus_for_tenant(&tenant))
        != AclDecision::Allow
    {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // ── Safety cap anti-DoS (ADN 5) : borne le volume d'accepted_ulids ─────────
    // Rejet AVANT toute lecture SQL et la boucle de validation : un body forgé ne
    // doit pas déclencher d'allocation/parse proportionnels à sa taille.
    if req.accepted_ulids.len() > MAX_ACCEPTED_ULIDS {
        return Err(GradatumError::InvalidInput(format!(
            "trop d'accepted_ulids ({} > {MAX_ACCEPTED_ULIDS})",
            req.accepted_ulids.len()
        )));
    }

    // ── Store présent ? Absent → recall_id introuvable (400) ───────────────────
    let Some(store) = state.proactive_recall.as_ref() else {
        return Err(GradatumError::InvalidInput(
            "proactive_recall store indisponible — recall_id introuvable".into(),
        ));
    };

    // ── `recall_id` existe ? Sinon 400 (id de session, pas un NoteId) ──────────
    let surfaced = store
        .get_surfaced(&req.recall_id, &tenant)
        .await
        .map_err(|e| {
            GradatumError::Storage(format!("proactive_recall_feedback get_surfaced: {e}"))
        })?
        .ok_or_else(|| {
            GradatumError::InvalidInput(format!("recall_id inconnu : {}", req.recall_id))
        })?;

    // ── Parse ULID + invariant `accepted ⊆ surfaced` ──────────────────────────
    let surfaced_set: HashSet<&str> = surfaced.iter().map(String::as_str).collect();
    for ulid in &req.accepted_ulids {
        Ulid::from_string(ulid)
            .map_err(|_| GradatumError::InvalidInput(format!("ULID accepté mal formé : {ulid}")))?;
        if !surfaced_set.contains(ulid.as_str()) {
            return Err(GradatumError::InvalidInput(format!(
                "ULID accepté hors surface présentée (accepted ⊄ surfaced) : {ulid}"
            )));
        }
    }

    // ── Enregistrement idempotent (UPSERT — 2× même feedback = 1 ligne) ────────
    let now_ms = chrono::Utc::now().timestamp_millis();
    store
        .record_feedback(&req.recall_id, &req.accepted_ulids, now_ms)
        .await
        .map_err(|e| {
            GradatumError::Storage(format!("proactive_recall_feedback record_feedback: {e}"))
        })?;

    // ── Télémétrie Task 12 : accepted = nombre d'ULIDs validés ────────────────
    state
        .metrics
        .proactive_accepted
        .inc_by(req.accepted_ulids.len() as u64);

    Ok(())
}

/// Lit la surface proactive pré-calculée du `tenant`.
///
/// Surface absente (store `None` ou tenant inconnu) → `Vec` vide, sans erreur.
///
/// # Errors
///
/// [`GradatumError::Storage`] sur échec SQL de `get_surface`.
async fn read_surface(state: &AppState, tenant: &str) -> Result<Vec<ProactiveHit>, GradatumError> {
    let Some(store) = state.proactive_surface.as_ref() else {
        tracing::debug!("proactive_recall: proactive_surface store absent (None) — surface vide");
        return Ok(Vec::new());
    };
    let surface = store
        .get_surface(tenant)
        .await
        .map_err(|e| GradatumError::Storage(format!("proactive_recall get_surface: {e}")))?
        .unwrap_or_default();
    Ok(surface)
}

/// Calcule les hits d'un rappel contextuel à la demande (retrieval RRF + hydratation).
///
/// `context` est passé brut à [`retrieve_candidates`] (sanitization FTS interne).
/// `sections` restricts retrieval to the requested set (cross-section no-leak — the filter
/// lives within `retrieve_candidates`).
///
/// # Errors
///
/// [`GradatumError`] sur échec SQL irrécupérable du retrieval ou de l'hydratation.
async fn contextual_hits(
    state: &AppState,
    tenant: &str,
    context: &str,
    sections: Option<&[String]>,
    limit: usize,
) -> Result<Vec<ProactiveHit>, GradatumError> {
    let vault_id = VaultId::new(tenant);
    // `Option<&[String]>` → `Option<Vec<&str>>` pour la signature de retrieve_candidates.
    let sections_owned: Option<Vec<&str>> =
        sections.map(|s| s.iter().map(String::as_str).collect());

    let outcome = retrieve_candidates(
        state,
        &vault_id,
        context,
        sections_owned.as_deref(),
        limit,
        state.context.embed_timeout_ms,
    )
    .await?;

    if outcome.candidates.is_empty() {
        return Ok(Vec::new());
    }

    let score_by_id: HashMap<&str, f64> = outcome
        .candidates
        .iter()
        .map(|c| (c.note_id.as_str(), c.rrf_score))
        .collect();
    let candidate_ids: Vec<String> = outcome
        .candidates
        .iter()
        .map(|c| c.note_id.clone())
        .collect();

    // Hydratation titre + section par batch (anti-N+1, comme proactive_refresh_once).
    let meta = state
        .search
        .get_titles_sections(vault_id.as_str(), &candidate_ids)
        .await?;

    // `snippet` vide en Task 10 (le corps n'est pas chargé ici — parité avec Task 8).
    let hits = candidate_ids
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
    Ok(hits)
}

/// Predicate ACL section (C3) — réutilise EXACTEMENT le mécanisme de `vault_context`.
///
/// Retourne `true` si l'appelant peut lire le locus `{tenant}/{section}` en `Read`.
fn section_readable(state: &AppState, trust: &TrustContext, tenant: &str, section: &str) -> bool {
    let locus = locus_for_section(tenant, Some(section));
    state.acl.evaluate(trust, AclOp::Read, &locus) == AclDecision::Allow
}
