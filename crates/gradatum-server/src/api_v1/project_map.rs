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
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::project_map::{ExportOptions, FeatureEntry, project_map_feature_entries};
use gradatum_core::trust::TrustContext;
use serde::Deserialize;

use crate::state::AppState;

/// Tenant unique (mono-vault v0.4.x).
const TENANT: &str = "main";

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

    // ── ACL : lecture sur la section project-map ──────────────────────────────
    let acl_locus = format!("{TENANT}/{SECTION}");
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // V-01 : audit-trail léger post-ACL (aucune valeur sensible dans le log).
    tracing::debug!(
        include_dropped = params.include_dropped,
        "export_features: accès accordé"
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
                "export_features: MAX_PAGES atteint — section project-map anormalement volumineuse. \
                 Export partiel retourné."
            );
            break;
        }

        let (records, total) = state
            .search
            .list_notes(TENANT, Some(SECTION), PAGE_SIZE, cursor.as_deref())
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
        "export_features: récupération terminée"
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
