//! `GET /api/v1/project-map/export-features` — Export JSON des cartes-feature.
//!
//! Endpoint HTTP authentifié exposant la liste des cartes-feature project-map,
//! identique au comportement de `gradatum-admin project-map export-features --json`.
//!
//! Utilisé par le gate CI T4 de `gradatum-www` pour vérifier la cohérence entre
//! la liste des features du miroir-site et la source de vérité vault gradatum.
//!
//! ## Contrat
//!
//! | Méthode | Path | Auth | Query | Réponse |
//! |---------|------|------|-------|---------|
//! | GET | `/api/v1/project-map/export-features` | Bearer JWT (Read) | `include_dropped` (bool, défaut `false`), `derived` (bool, défaut `true`) | `200 application/json` — tableau `[FeatureEntry]` |
//!
//! - Auth : même middleware Bearer JWT que les autres routes `/api/v1`.
//! - `derived` (défaut `true`) : `release`/`version` dérivés du `[[track:]]`. Le
//!   stocké ayant été retiré, la dérivation est l'unique voie qui projette encore les features.
//!   `derived=false` = voie stockée pure (compat/diagnostic, de facto vide post-retrait).
//! - `include_dropped=true` : expose les cartes `release:dropped` (audit complet).
//! - Défaut (`include_dropped=false`) : miroir-site — inclut backlog (version `"vX.Y.Z"`),
//!   exclut uniquement `release:dropped` (Règle A NOMENCLATURE §10e).
//! - Tri : identifiants F-XX croissants numériques.
//!
//! ## Architecture DRY
//!
//! La projection est déléguée à
//! [`gradatum_core::project_map::project_map_feature_entries`] — SSOT partagée
//! avec `gradatum-admin project-map export-features`.
//! Le handler récupère les notes section `project-map` via `state.search.list_notes`,
//! puis passe les paires `(body_text, title)` à la fonction de projection pure.
//!
//! ## Pagination
//!
//! `list_notes` est clampé à 200 notes par page. Ce handler déroule la pagination
//! cursor-based en boucle pour récupérer la totalité de la section. La borne
//! `MAX_PAGES` (50 → 10 000 notes) protège contre un runaway. Le `total` retourné
//! par la première requête fixe la borne haute naturelle de la boucle.
//!
//! ## Filtre garbage (parité CLI)
//!
//! La CLI admin (`project_map_export.rs`) exclut `status != 'downgraded' AND status != 'garbage'`.
//! `list_notes` n'exclut que `downgraded` — les cartes en `garbage` (GC pending)
//! sont filtrées côté handler pour garantir la parité CLI↔HTTP.
//!
//! ## Erreurs
//!
//! - `401 Unauthorized` : requête non authentifiée.
//! - `403 Forbidden` : ACL Read refusée sur `main/project-map`.
//! - `500 Internal Server Error` : erreur de stockage index.

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use gradatum_core::project_map::{
    DerivationFallbackReason, ExportOptions, FeatureEntry, ProjectMapCardEntry,
    project_map_card_index, project_map_feature_entries,
    project_map_feature_entries_derived_scoped,
};
use gradatum_core::scope::VaultId;
use gradatum_core::trust::TrustContext;
use serde::Deserialize;

use crate::state::AppState;

/// Vault namespace ciblé par ce handler (dimension NAMESPACE, distincte du
/// principal `TenantId`).
///
/// Déploiement single-vault : toujours `main`. Point de résolution **typé**
/// remplaçant l'ancien `const TENANT: &str` — en multi-vault (Groupe B) il deviendra
/// un routage par registre.
#[must_use]
pub fn target_vault() -> VaultId {
    VaultId::new("main")
}

/// Section cible pour l'export project-map.
const SECTION: &str = "project-map";

/// Taille de page pour la pagination cursor de `list_notes`.
///
/// `list_notes` clamp effectif = 200 (crate index `sqlite.rs:1822`). On passe
/// cette valeur explicitement pour rester aligné — toute valeur > 200 serait
/// silencieusement clampée à 200 par la crate index (qu'on ne modifie pas).
const PAGE_SIZE: usize = 200;

/// Nombre maximum de pages avant abandon avec warning.
///
/// 50 pages × 200 notes/page = 10 000 notes max — bien au-delà de ce que
/// la section project-map peut contenir (croissance O(centaines) à l'horizon v1.0).
/// Guard anti-runaway pour protéger contre un état de DB anormal.
const MAX_PAGES: usize = 50;

/// Query parameters de `GET /api/v1/project-map/export-features`.
#[derive(Debug, Deserialize)]
pub struct ExportFeaturesQuery {
    /// Si `true`, inclut les cartes `release:dropped` (audit complet).
    /// Défaut `false` : miroir-site — exclut uniquement `dropped`,
    /// inclut les cartes backlog (version `"vX.Y.Z"`) (Règle A).
    #[serde(default)]
    pub include_dropped: bool,
    /// Dérive `release` **et** `version` depuis le pointeur `[[track:]]` au lieu du
    /// `[[release:]]`/`[[version:]]` stocké.
    ///
    /// **Défaut `true` depuis le retrait irréversible du stocké** : les cartes de
    /// travail ne portent plus de `[[release:]]`/`[[version:]]`, la dérivation est donc l'unique
    /// source qui projette encore les features. `derived=false` sélectionne la voie stockée pure
    /// (conservée pour compat/diagnostic) — devenue de facto vide post-retrait. Les cartes
    /// indérivables sont journalisées — jamais de repli silencieux.
    #[serde(default = "default_derived")]
    pub derived: bool,
}

/// Valeur par défaut de [`ExportFeaturesQuery::derived`] : `true` depuis le retrait du stocké.
/// La dérivation est désormais la voie nominale de l'export.
const fn default_derived() -> bool {
    true
}

impl Default for ExportFeaturesQuery {
    /// Reflète les défauts serde : `include_dropped = false`, `derived = true` (voie nominale
    /// post-retrait). Garde `ExportFeaturesQuery::default()` cohérent avec une requête sans query
    /// string.
    fn default() -> Self {
        Self {
            include_dropped: false,
            derived: default_derived(),
        }
    }
}

/// Récupère **toutes** les notes de la section `project-map` du vault, en déroulant la pagination
/// cursor de `list_notes` (clampée à 200/page côté crate index).
///
/// SSOT de récupération partagée par [`export_features`] et [`list_cards`]. Le filtre garbage
/// (parité CLI) reste à la charge de l'appelant : ce helper rend les enregistrements bruts (hors
/// `downgraded`, déjà exclus par `list_notes`).
///
/// `caller` n'apparaît que dans les logs (contexte de diagnostic).
///
/// # Errors
///
/// `500 Internal Server Error` si `list_notes` échoue sur une page.
async fn fetch_all_project_map_records(
    state: &AppState,
    vault: &str,
    caller: &str,
) -> Result<Vec<gradatum_core::index::NoteRecord>, StatusCode> {
    // list_notes clamp le limit à 200 (sqlite.rs:1822) — on pagine explicitement.
    // Stratégie : première requête fixe `total`, boucle jusqu'à épuisement.
    // Guard anti-runaway : MAX_PAGES (50 pages = 10 000 notes).
    let mut all_records = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages_fetched: usize = 0;

    loop {
        if pages_fetched >= MAX_PAGES {
            // Anomalie opérationnelle : runaway guard atteint — log et arrêt propre.
            tracing::warn!(
                caller,
                pages = pages_fetched,
                max_pages = MAX_PAGES,
                collected = all_records.len(),
                section = SECTION,
                "fetch_all_project_map_records: MAX_PAGES reached — abnormally large \
                 project-map section. Partial result returned."
            );
            break;
        }

        let (records, total) = state
            .search
            .list_notes(vault, Some(SECTION), PAGE_SIZE, cursor.as_deref())
            .await
            .map_err(|e| {
                tracing::error!(
                    caller,
                    err = %e,
                    section = SECTION,
                    page = pages_fetched,
                    "fetch_all_project_map_records: list_notes failed"
                );
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        let page_len = records.len();
        pages_fetched += 1;

        // Cursor suivant = dernier ULID de la page (tri ASC ULID dans list_notes).
        if let Some(last) = records.last() {
            cursor = Some(last.id.clone());
        }

        all_records.extend(records);

        // Épuisement naturel : page incomplète OU total atteint.
        let done = page_len < PAGE_SIZE || all_records.len() >= total as usize;
        if done {
            break;
        }
    }

    tracing::debug!(
        caller,
        section = SECTION,
        pages = pages_fetched,
        raw_count = all_records.len(),
        "fetch_all_project_map_records: retrieval complete"
    );

    Ok(all_records)
}

/// `GET /api/v1/project-map/export-features?include_dropped=<bool>`
///
/// Retourne la liste complète des cartes-feature project-map triées par F-XX croissant.
///
/// ## Auth
///
/// Bearer JWT avec scope `Read` sur `main/project-map`. Sans bearer valide → 401.
///
/// ## Pagination
///
/// Déroulée en interne via le cursor ULID de `list_notes` (200 notes/page max) —
/// le client reçoit toujours la liste complète en une seule réponse.
///
/// ## Filtre garbage
///
/// Les cartes en statut `garbage` (GC pending) sont exclues pour correspondre
/// au comportement de la CLI admin (`status != 'downgraded' AND status != 'garbage'`).
///
/// ## Projection
///
/// Délègue à `project_map_feature_entries` (SSOT `gradatum-core`) — comportement
/// identique à la CLI admin.
///
/// # Errors
///
/// - `401 Unauthorized` : requête non authentifiée.
/// - `403 Forbidden` : ACL Read refusée sur `main/project-map`.
/// - `500 Internal Server Error` : erreur de stockage index.
pub async fn export_features(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Query(params): Query<ExportFeaturesQuery>,
) -> Result<Json<Vec<FeatureEntry>>, StatusCode> {
    // ── Authentification ──────────────────────────────────────────────────────
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // ── T9 (A3-handlers) : OFF = ACL Read legacy sur `main/project-map` (byte-identical) ;
    //    ON = vault effectif du principal JWT (ACL cible + grant read + statut actif). ──
    let vault =
        crate::api_v1::tenant_guard::resolve_read_vault(&state, &trust, target_vault(), SECTION)
            .await?;

    // V-01 : audit-trail léger post-ACL (aucune valeur sensible dans le log).
    tracing::debug!(
        include_dropped = params.include_dropped,
        "export_features: access granted"
    );

    // ── Récupération paginée de toutes les notes project-map ─────────────────
    let all_records =
        fetch_all_project_map_records(&state, vault.as_str(), "export_features").await?;

    // ── C2 : filtre garbage (parité CLI admin) ────────────────────────────────
    //
    // list_notes exclut status='downgraded' mais PAS 'garbage'.
    // La CLI admin exclut les deux (`project_map_export.rs:62-63`).
    // On aligne ici pour garantir CLI↔HTTP identiques sur la même DB.
    let notes: Vec<(String, String)> = all_records
        .into_iter()
        .filter(|r| r.status != "garbage")
        .map(|r| (r.body_text, r.title.unwrap_or_default()))
        .collect();

    // ── Projection pure → Vec<FeatureEntry> ──────────────────────────────────
    //
    // Transformation déléguée à gradatum-core (SSOT partagée avec admin CLI).
    let opts = ExportOptions {
        include_dropped: params.include_dropped,
    };

    // Make-before-break (F-184 Phase 6) : `derived=true` dérive `release` du pointeur `[[track:]]`
    // (voie parallèle) ; défaut = lecture du `[[release:]]` stocké (byte-identique à l'existant).
    let entries = if params.derived {
        let out = project_map_feature_entries_derived_scoped(&notes, opts, None);
        // Échec de dérivation : VISIBLE (jamais silencieux, jamais de panic — P2-A). Un `NoTrack`
        // est attendu pendant la fenêtre additive → debug ; les autres cas sont des anomalies → warn.
        // Post-retrait (Phase 7), `stored: None` signifie carte IGNORÉE (ni dérivable ni stockée) —
        // une anomalie de registre (pointeur `[[track:]]` pendant).
        for d in &out.diagnostics {
            let outcome = if d.stored.is_some() {
                "fell back to stored release"
            } else {
                "skipped — no stored release either (dangling track anomaly)"
            };
            match &d.reason {
                DerivationFallbackReason::NoTrack if d.stored.is_some() => tracing::debug!(
                    feature = %d.feature,
                    stored = ?d.stored,
                    "export_features(derived): no track pointer — {outcome}"
                ),
                DerivationFallbackReason::NoTrack => tracing::warn!(
                    feature = %d.feature,
                    stored = ?d.stored,
                    "export_features(derived): no track pointer — {outcome}"
                ),
                DerivationFallbackReason::Unresolved(e) => tracing::warn!(
                    feature = %d.feature,
                    stored = ?d.stored,
                    err = %e,
                    "export_features(derived): track unresolved — {outcome}"
                ),
                DerivationFallbackReason::Undetermined(e) => tracing::warn!(
                    feature = %d.feature,
                    stored = ?d.stored,
                    err = %e,
                    "export_features(derived): structure status undeterminable — {outcome}"
                ),
                // `DerivationFallbackReason` est #[non_exhaustive] : toute variante future
                // reste VISIBLE (warn), jamais silencieuse.
                other => tracing::warn!(
                    feature = %d.feature,
                    stored = ?d.stored,
                    reason = ?other,
                    "export_features(derived): derivation failure — {outcome}"
                ),
            }
        }
        out.entries
    } else {
        project_map_feature_entries(&notes, opts)
    };

    Ok(Json(entries))
}

/// Query parameters de `GET /api/v1/project-map/cards`.
#[derive(Debug, Default, Deserialize)]
pub struct ListCardsQuery {
    /// Filtre optionnel par **version**. Accepte `2.1.0` ou `v2.1.0` (le préfixe `v` est
    /// normalisé). Une carte de travail matche si son `[[track:]]` vise cette version ; une carte
    /// de structure (ROADMAP/BACKLOG) matche si son propre `[[version:]]` la porte — le listing
    /// d'un jalon inclut donc sa propre carte de structure (F-211). Absent = toutes les cartes.
    #[serde(default)]
    pub version: Option<String>,
}

/// `GET /api/v1/project-map/cards?version=<v>`
///
/// Listage sanctionné des cartes project-map en **une seule requête**, tous axes d'identification
/// **nommés** (F-211, supersedes F-253). Résout le manque de longue date : il n'existait aucune
/// voie sanctionnée pour lister les cartes d'un jalon — cela passait par un export + filtre manuel
/// ou par l'« oracle de sous-chaîne faux » (recherche d'une chaîne de version).
///
/// Rend, par carte (travail **et** structure) : identifiant (ULID), numéro `F-XX`, statut, type,
/// release (dérivé du `[[track:]]` — jamais le stocké retiré), version, visibilité,
/// titre, et les rôles de dépendance (`supersedes`, `parent`, `track`). Contrairement à
/// [`export_features`] (miroir-site, `FEATURE`-only, axes pauvres), ce listing **n'applique aucun
/// filtre miroir** : chaque carte du périmètre est rendue avec chaque axe exposé, pour qu'un
/// appelant filtre sur les colonnes **nommées** plutôt que sur un prédicat caché (criteria 7, 8).
///
/// ## Contrat
///
/// | Méthode | Path | Auth | Query | Réponse |
/// |---------|------|------|-------|---------|
/// | GET | `/api/v1/project-map/cards` | Bearer JWT (Read) | `version` (str, opt) | `200` — `[ProjectMapCardEntry]` |
///
/// Tri : cartes de structure d'abord, puis cartes de travail par `F-XX` croissant (égalités par
/// ULID). Une version inexistante rend `[]`. Une carte de travail au `[[track:]]` irrésolu est
/// listée avec `release: null` (le null est le signal visible de l'anomalie, jamais un drop).
///
/// # Errors
///
/// - `401 Unauthorized` : requête non authentifiée.
/// - `403 Forbidden` : ACL Read refusée sur `main/project-map`.
/// - `500 Internal Server Error` : erreur de stockage index.
pub async fn list_cards(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Query(params): Query<ListCardsQuery>,
) -> Result<Json<Vec<ProjectMapCardEntry>>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let vault =
        crate::api_v1::tenant_guard::resolve_read_vault(&state, &trust, target_vault(), SECTION)
            .await?;

    tracing::debug!(
        version = ?params.version,
        "list_cards: access granted"
    );

    let all_records = fetch_all_project_map_records(&state, vault.as_str(), "list_cards").await?;

    // Filtre garbage (parité CLI admin, identique à export_features) : list_notes exclut
    // `downgraded` mais pas `garbage`. On aligne pour que le décompte rendu égale le périmètre
    // (criterion 5). Triples `(id, body, title)` — l'ULID est requis pour l'axe identifiant.
    let notes: Vec<(String, String, String)> = all_records
        .into_iter()
        .filter(|r| r.status != "garbage")
        .map(|r| (r.id, r.body_text, r.title.unwrap_or_default()))
        .collect();

    let entries = project_map_card_index(&notes, params.version.as_deref(), Some("gradatum"));

    Ok(Json(entries))
}

/// `POST /api/v1/project-map/create-feature`
///
/// Crée une carte-feature project-map dont le **numéro est choisi par le serveur**. Le corps
/// de requête est un [`crate::api_v1::dto::CreateFeatureCardRequest`] (titre + corps SANS
/// rôle `[[feature:…]]` + les 5 autres rôles). Réponse `200` :
/// `{ "feature": "F-135", "number": 135, "job_id", "note_id", "poll_url" }` — l'écriture est
/// asynchrone, confirmer via `job_status`.
///
/// Thin wrapper — délègue à [`crate::api_v1::logic::create_feature_card_impl`], qui porte
/// l'ACL (Bearer JWT `Write` sur `{tenant}/main`), l'allocation atomique et l'enqueue.
///
/// # Errors
///
/// - `400 Bad Request` : corps portant déjà un `[[feature:…]]`, ou carte incomplète.
/// - `401 Unauthorized` : requête non authentifiée.
/// - `403 Forbidden` : ACL Write refusée / cross-tenant.
/// - `409 Conflict` / `500 Internal Server Error` : échec allocation ou enqueue.
pub async fn create_feature(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    axum::extract::Json(req): axum::extract::Json<crate::api_v1::dto::CreateFeatureCardRequest>,
) -> Result<
    (
        StatusCode,
        Json<crate::api_v1::dto::CreateFeatureCardResponse>,
    ),
    StatusCode,
> {
    let request_id = ulid::Ulid::generate().to_string();
    crate::api_v1::logic::create_feature_card_impl(&state, &trust, req, &request_id)
        .await
        // 202 Accepted : le numéro est attribué, l'écriture de la carte est asynchrone
        // (poll via `poll_url` / `job_status`).
        .map(|resp| (StatusCode::ACCEPTED, Json(resp)))
        .map_err(|e| crate::api_v1::logic::err_to_status(&e))
}
