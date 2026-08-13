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
//! | GET | `/api/v1/project-map/export-features` | Bearer JWT (Read) | `include_dropped` (bool, défaut `false`) | `200 application/json` — tableau `[FeatureEntry]` |
//!
//! - Auth : même middleware Bearer JWT que les autres routes `/api/v1`.
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
use gradatum_core::project_map::{ExportOptions, FeatureEntry, project_map_feature_entries};
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
#[derive(Debug, Deserialize, Default)]
pub struct ExportFeaturesQuery {
    /// Si `true`, inclut les cartes `release:dropped` (audit complet).
    /// Défaut `false` : miroir-site — exclut uniquement `dropped`,
    /// inclut les cartes backlog (version `"vX.Y.Z"`) (Règle A).
    #[serde(default)]
    pub include_dropped: bool,
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
    //
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
                pages = pages_fetched,
                max_pages = MAX_PAGES,
                collected = all_records.len(),
                section = SECTION,
                "export_features: MAX_PAGES reached — abnormally large project-map section. \
                 Partial export returned."
            );
            break;
        }

        let (records, total) = state
            .search
            .list_notes(vault.as_str(), Some(SECTION), PAGE_SIZE, cursor.as_deref())
            .await
            .map_err(|e| {
                tracing::error!(
                    err = %e,
                    section = SECTION,
                    page = pages_fetched,
                    "export_features: list_notes failed"
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
        section = SECTION,
        pages = pages_fetched,
        raw_count = all_records.len(),
        "export_features: retrieval complete"
    );

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

    let entries = project_map_feature_entries(&notes, opts);

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
