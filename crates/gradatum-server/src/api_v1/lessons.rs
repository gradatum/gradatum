//! `GET /api/v1/lessons/recall` — Lesson recall endpoint.
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
    extract::{Query, State},
    http::StatusCode,
    Extension, Json,
};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::scope::VaultId;
use gradatum_core::trust::TrustContext;
use gradatum_dto::{is_valid_lesson_class, LessonHit, LessonsRecallRequest, LessonsRecallResponse};

use crate::state::AppState;

/// Fixed section for the lesson corpus.
const LESSONS_SECTION: &str = "lessons-learned";

/// Single-vault tenant — aligned with `vault_status`.
const TENANT: &str = "main";

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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // ACL : lecture sur la section lessons-learned.
    let acl_locus = format!("{TENANT}/{LESSONS_SECTION}");
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // Validation vocabulaire contrôlé = anti-injection FTS. La classe est
    // transmise littéralement au moteur FTS5 (quotée en phrase côté index), mais
    // on rejette d'abord tout ce qui sort des 12 valeurs fermées.
    let class = params.class.trim();
    if !is_valid_lesson_class(class) {
        tracing::warn!(class = %class, "lessons_recall: classe hors vocabulaire contrôlé");
        return Err(StatusCode::BAD_REQUEST);
    }

    // limit : défaut 5, borné [1, 20].
    let limit = params.limit.unwrap_or(5).clamp(1, 20) as usize;

    let vault_id = VaultId::new(TENANT);
    let raw_hits = state
        .search
        .recall_lessons(&vault_id, class, limit)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, class = %class, "lessons_recall: recall_lessons failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let items: Vec<LessonHit> = raw_hits
        .into_iter()
        .map(|h| LessonHit {
            ulid: h.note_id.0.to_string(),
            title: h.title.unwrap_or_default(),
            snippet: h.snippet,
            tags: h.tags,
            anchor_ms: h.anchor_ms,
        })
        .collect();

    Ok(Json(LessonsRecallResponse { items }))
}
