//! Sub-router for `/api/v1` — MCP read/write routes, notes sync, jobs, and history.
//!
//! Built via [`router`] and nested under `/api/v1` by `crate::router::build_router`.
//!
//! # Read routes
//!
//! | Method | Path | Handler |
//! |--------|------|---------|
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
//! # Async write routes (202 Accepted)
//!
//! | Method | Path | Handler |
//! |--------|------|---------|
//! | POST | `/vault_write`    | [`write::vault_write`] |
//! | POST | `/vault_classify` | [`write::vault_classify`] |
//!
//! # Sync note routes (200/204)
//!
//! | Method | Path | Handler |
//! |--------|------|---------|
//! | POST  | `/vault_downgrade` | `notes::vault_downgrade` |
//! | PATCH | `/notes/{id}`      | `notes::patch_note` |
//!
//! # History routes (synchronous 200 OK)
//!
//! | Method | Path | ACL | Handler |
//! |--------|------|-----|---------|
//! | POST | `/vault_history`     | Read  | [`history::vault_history`] |
//! | POST | `/vault_history_get` | Read  | [`history::vault_history_get`] |
//! | POST | `/vault_restore`     | Write | [`history::vault_restore`] |
//! | POST | `/vault_diff`        | Read  | [`history::vault_diff`] |
//!
//! # Lesson recall route (GET, BM25-only, no LLM)
//!
//! | Method | Path | ACL | Handler |
//! |--------|------|-----|---------|
//! | GET | `/lessons/recall` | Read | [`lessons::lessons_recall`] |
//!
//! # Legacy job poll route
//!
//! | Method | Path | Handler |
//! |--------|------|---------|
//! | GET | `/jobs/{id}` | [`jobs::get_job`] (deprecated — use `/jobs/v2/{id}`) |
//!
//! # Job routes
//!
//! | Method | Path | Handler |
//! |--------|------|---------|
//! | GET  | `/jobs`             | [`jobs_v2::list_jobs`] — cursor-based paginated list |
//! | POST | `/jobs`             | [`jobs_v2::create_job`] — create with `Idempotency-Key` |
//! | GET  | `/jobs/{id}/v2`     | [`jobs_v2::get_job_v2`] — detail |
//! | POST | `/jobs/{id}/cancel` | [`jobs_v2::cancel_job`] — cancellation |
//! | GET  | `/jobs/{id}/events` | [`jobs_v2::job_events`] — SSE stream |
//!
//! Fixed routes (`/jobs`) are defined BEFORE parametric routes (`/jobs/{id}`).
//! Parametric sub-routes (`/jobs/{id}/cancel`, `/jobs/{id}/events`) follow
//! the fixed-before-parametric ordering rule.

pub mod code_scope;
pub mod dashboard;
pub mod dto;
pub mod event_log;
pub mod forget;
pub mod handlers;
pub mod history;
pub mod jobs;
pub mod jobs_v2;
pub mod lessons;
pub mod notes;
pub mod review;
pub mod session_log;
pub(crate) mod tenant_guard;
pub mod timeline;
pub mod write;

use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};

use crate::state::AppState;

/// Builds and returns the `/api/v1` sub-router.
///
/// Nested under `/api/v1` by the main router (`build_router`).
/// [`AppState`] is injected via `Router::with_state`.
///
/// [`TrustContext`] is extracted by the `trust_layer` middleware (configured in
/// the main router) and available via `Extension<TrustContext>` in each handler.
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
        // V2 — body limit 4 KiB : payload nominal <512 o (filtres + cursor ULID).
        // Protège contre un flood avant parsing JSON (pattern vault_forget/event-log).
        .route(
            "/vault_timeline",
            post(timeline::vault_timeline).layer(DefaultBodyLimit::max(4 * 1024)),
        )
        // Routes POST write async (T3 P2.0b — async 202 Accepted)
        .route("/vault_write", post(write::vault_write))
        .route("/vault_classify", post(write::vault_classify))
        // ── F-40 History CoW (v0.4.0 — synchrones 200 OK) ────────────────────
        // Règle fixed-before-parametric respectée : toutes les routes fixes.
        .route("/vault_history", post(history::vault_history))
        .route("/vault_history_get", post(history::vault_history_get))
        .route("/vault_restore", post(history::vault_restore))
        .route("/vault_diff", post(history::vault_diff))
        // ── F-44 Forget sémantique ────────────────────────────────────────────
        // Règle fixed-before-parametric :
        // - POST /vault_forget (fixe) avant /vault/unforgot/{ulid} (paramétrique)
        // - GET /vault/forgotten (fixe)
        //
        // C3 — body limit 1 MiB : protège contre un flood de confirm_ulids larges.
        // Calcul : 200 ULIDs × 26 chars + overhead JSON ≈ ~6 KB nominal.
        // 1 MiB offre un facteur ×160 de tolérance (cohérent avec le cap 200 ulids C8).
        .route(
            "/vault_forget",
            post(forget::vault_forget).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        .route("/vault/forgotten", get(forget::vault_forgotten_list))
        .route("/vault/unforgot/{ulid}", post(forget::vault_unforgot))
        // ── F-60 Lesson Recall (v0.4.4) — GET fixe, BM25-only, aucun LLM ─────
        .route("/lessons/recall", get(lessons::lessons_recall))
        // ── F-61 Code Scope (v0.5.2 Phase C) — POST fixe, BM25-only, endpoint dédié ─
        //
        // Body limit 4 KiB : payload nominal <1 KB (vault + selector + budget).
        // Endpoint dédié bypassant la garde mono-vault — invariant sécu N°1 dans le handler.
        .route(
            "/code_scope",
            post(code_scope::code_scope).layer(DefaultBodyLimit::max(4 * 1024)),
        )
        // ── F-37 S1.2 Review queue (v0.4.6) — GET fixe, auth Read ────────────
        .route("/review", get(review::list_review))
        // ── F-37 S1.3 Dashboard (v0.4.6) — GET fixe, auth Read ───────────────
        .route("/dashboard", get(dashboard::dashboard))
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
        // ── session-log Tier 1 (council Art.15bis 2026-06-12) — append-only ──
        //
        // Body limit 4 KiB : payload nominal <1 KB (intent≤200, target≤512, refs
        // courts). Protège contre un flood avant parsing JSON (pattern vault_timeline).
        .route(
            "/session-log/trace",
            post(session_log::post_session_trace).layer(DefaultBodyLimit::max(4 * 1024)),
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
