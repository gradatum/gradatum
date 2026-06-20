//! `GET /api/v1/lessons/recall` — Lesson recall endpoint — thin wrapper sur `logic::lessons_recall_impl`.
//!
//! Recalls lessons by **class** from the controlled vocabulary.
//! Search is **BM25-only** (lexical, no LLM call), restricted to the
//! `lessons-learned` section, and excludes lessons tagged `codified`.
//!
//! # Contract
//!
//! | Method | Path | Query params | Response | Codes |
//! |--------|------|--------------|----------|-------|
//! | GET | `/api/v1/lessons/recall` | `class` (required), `limit` (optional, default 5) | `LessonsRecallResponse` | 200 / 400 / 401 / 403 / 500 |
//!
//! - `class`: validated against [`gradatum_dto::LESSON_CLASSES`] → **400** if
//!   outside the vocabulary. This validation also guards against FTS injection:
//!   only the 12 closed literal values reach the search engine.
//! - `limit`: clamped to `[1, 20]`, default `5`.
//! - Auth: standard JWT (Read), same middleware as all other `/api/v1` routes.
//!
//! # Performance
//!
//! Purely FTS5 code path (no embedding, no RRF, no reranker). Target: <50 ms
//! on the normalized lesson corpus.

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use gradatum_core::trust::TrustContext;
use gradatum_dto::{LessonsRecallRequest, LessonsRecallResponse};

use crate::state::AppState;

/// `GET /api/v1/lessons/recall?class=<x>&limit=<n>`
///
/// Returns up to `limit` lessons of the requested class, ranked by BM25 relevance.
/// See the module documentation for the full contract.
///
/// # Errors
///
/// - `401 Unauthorized`: unauthenticated request.
/// - `403 Forbidden`: ACL Read denied on `main/lessons-learned`.
/// - `400 Bad Request`: `class` is not in the controlled vocabulary.
/// - `500 Internal Server Error`: index storage failure.
pub async fn lessons_recall(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Query(params): Query<LessonsRecallRequest>,
) -> Result<Json<LessonsRecallResponse>, StatusCode> {
    crate::api_v1::logic::lessons_recall_impl(&state, &trust, params)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "lessons_recall: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}
