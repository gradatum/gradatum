//! Sous-router `/api/v1` — routes MCP read/write + notes sync + jobs F-16.
//!
//! Le routeur est construit via [`router`] et nesté sous `/api/v1` dans
//! `crate::router::build_router`.
//!
//! # Routes Read (T8 P2.0a)
//!
//! | Méthode | Path | Handler |
//! |---------|------|---------|
//! | GET  | `/vault_status`  | [`handlers::vault_status`] |
//! | GET  | `/vault_authors` | [`handlers::vault_authors`] |
//! | GET  | `/vault_tags`    | [`handlers::vault_tags`] |
//! | POST | `/vault_search`  | [`handlers::vault_search`] |
//! | POST | `/vault_read`    | [`handlers::vault_read`] |
//! | POST | `/vault_list`    | [`handlers::vault_list`] |
//! | POST | `/vault_graph`   | [`handlers::vault_graph`] |
//! | POST | `/vault_links`   | [`handlers::vault_links`] |
//! | POST | `/vault_trace`   | [`handlers::vault_trace`] |
//! | POST | `/vault_context` | [`handlers::vault_context`] |
//!
//! # Routes Write async (T3 P2.0b — async 202 Accepted)
//!
//! | Méthode | Path | Handler |
//! |---------|------|---------|
//! | POST | `/vault_write`    | [`write::vault_write`] |
//! | POST | `/vault_classify` | [`write::vault_classify`] |
//!
//! # Routes Notes sync (Phase 2.1.2 alpha.9 — synchrones 200/204)
//!
//! | Méthode | Path | Handler |
//! |---------|------|---------|
//! | POST  | `/vault_downgrade` | [`notes::vault_downgrade`] |
//! | PATCH | `/notes/{id}`      | [`notes::patch_note`] |
//!
//! # Routes Jobs v1 Poll (T3 P2.0b — handler legacy)
//!
//! | Méthode | Path | Handler |
//! |---------|------|---------|
//! | GET | `/jobs/{id}` | [`jobs::get_job`] (déprécié — utiliser `/jobs/v2/{id}`) |
//!
//! # Routes Jobs F-16 (Phase 3 v0.2.0)
//!
//! | Méthode | Path | Handler |
//! |---------|------|---------|
//! | GET  | `/jobs`                | [`jobs_v2::list_jobs`] — liste paginée cursor-based |
//! | POST | `/jobs`                | [`jobs_v2::create_job`] — création avec Idempotency-Key |
//! | GET  | `/jobs/{id}/v2`        | [`jobs_v2::get_job_v2`] — détail (fix E-12) |
//! | POST | `/jobs/{id}/cancel`    | [`jobs_v2::cancel_job`] — annulation |
//! | GET  | `/jobs/{id}/events`    | [`jobs_v2::job_events`] — SSE stream |
//!
//! Note : les routes fixes (`/jobs`) sont définies AVANT les routes paramétriques
//! (`/jobs/{id}`). Les sous-routes paramétriques (`/jobs/{id}/cancel`,
//! `/jobs/{id}/events`) respectent l'ordre fixed-before-parametric.

pub mod dto;
pub mod event_log;
pub mod handlers;
pub mod jobs;
pub mod jobs_v2;
pub mod notes;
pub mod write;

use axum::{
    routing::{get, post},
    Router,
};

use crate::state::AppState;

/// Construit et retourne le sous-routeur `/api/v1`.
///
/// Ce routeur est nesté sous `/api/v1` par le routeur principal (`build_router`).
/// L'[`AppState`] est injecté via `Router::with_state`.
///
/// Le [`TrustContext`] est extrait par le middleware `trust_layer` (configuré dans
/// le routeur principal) et disponible via `Extension<TrustContext>` dans chaque handler.
///
/// [`TrustContext`]: gradatum_core::trust::TrustContext
pub fn router() -> Router<AppState> {
    Router::new()
        // Routes GET fixes — définies AVANT les routes paramétriques (fixed-before-parametric).
        .route("/vault_status", get(handlers::vault_status))
        .route("/vault_authors", get(handlers::vault_authors))
        .route("/vault_tags", get(handlers::vault_tags))
        // Routes POST read (T8 P2.0a)
        .route("/vault_search", post(handlers::vault_search))
        .route("/vault_read", post(handlers::vault_read))
        .route("/vault_list", post(handlers::vault_list))
        .route("/vault_graph", post(handlers::vault_graph))
        .route("/vault_links", post(handlers::vault_links))
        .route("/vault_trace", post(handlers::vault_trace))
        .route("/vault_context", post(handlers::vault_context))
        // Routes POST write async (T3 P2.0b — async 202 Accepted)
        .route("/vault_write", post(write::vault_write))
        .route("/vault_classify", post(write::vault_classify))
        // Route event-log ingestion (B1 tranche v0.3.0) — append-only, gateway sink.
        //
        // F3 : body limité à 2MB avant parsing JSON (protection anti-DOS).
        // 1000 events × ~200 octets/event = ~200KB nominal. 2MB offre un facteur
        // ×10 de tolérance sans permettre l'envoi de payloads multi-dizaines de MB.
        .route(
            "/event-log",
            post(event_log::post_event_log)
                .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        // Routes notes sync (Phase 2.1.2 alpha.9) — merge sous-routeur notes
        // Note : vault_downgrade (fixe) est défini AVANT jobs/{id} et notes/{id} (paramétriques)
        .merge(notes::router())
        // ── F-16 Jobs API (Phase 3 v0.2.0) ──────────────────────────────────
        // Règle fixed-before-parametric : GET/POST /jobs (fixe) avant /jobs/{id} (paramétrique).
        .route("/jobs", get(jobs_v2::list_jobs).post(jobs_v2::create_job))
        // Route legacy jobs poll (T3 P2.0b) — conservée pour rétrocompat
        // GET /jobs/{id} → jobs::get_job (ancien handler, i64 ID)
        // GET /jobs/{id}/v2 → jobs_v2::get_job_v2 (nouveau handler, ULID, fix E-12)
        .route("/jobs/{id}/v2", get(jobs_v2::get_job_v2))
        .route("/jobs/{id}/cancel", post(jobs_v2::cancel_job))
        .route("/jobs/{id}/events", get(jobs_v2::job_events))
        // Legacy : GET /jobs/{id} (ancien handler i64 — conservé pour rétrocompat P2.0b)
        .route("/jobs/{id}", get(jobs::get_job))
}
