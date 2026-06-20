//! Handler for `POST /v1/rerank` — cross-encoder reranking.
//!
//! Uses `Arc<dyn Reranker>` from `AppState.reranker` to re-order a list of
//! passages by relevance to a query.
//!
//! Request format:
//! ```json
//! {
//!   "query": "my question",
//!   "documents": ["doc1", "doc2", "doc3"],
//!   "top_n": 2
//! }
//! ```
//!
//! Response format:
//! ```json
//! {
//!   "results": [
//!     { "index": 2, "document": "doc3", "relevance_score": 0.93 },
//!     { "index": 0, "document": "doc1", "relevance_score": 0.71 }
//!   ]
//! }
//! ```
//!
//! Error codes:
//! - 400: invalid request (empty query, empty documents)
//! - 503: reranker not configured
//! - 502: reranker backend error

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::{AppState, commons::error::LlmError, error::ApiError};

/// Request body for `POST /v1/rerank`.
#[derive(Debug, Deserialize)]
pub struct RerankRequest {
    /// Search query.
    pub query: String,
    /// Documents to re-order; each entry is a plain text string.
    pub documents: Vec<String>,
    /// Maximum number of results to return (optional — returns all when absent).
    #[serde(default)]
    pub top_n: Option<usize>,
}

/// A single reranking result.
#[derive(Debug, Serialize)]
pub struct RerankResult {
    /// Zero-based index in the original `documents` array.
    pub index: usize,
    /// Document content.
    pub document: String,
    /// Relevance score (0.0 to 1.0).
    pub relevance_score: f32,
}

/// Response body for `POST /v1/rerank`.
#[derive(Debug, Serialize)]
pub struct RerankResponse {
    pub results: Vec<RerankResult>,
}

/// Handler for `POST /v1/rerank`.
///
/// Uses the reranker configured in `AppState` to score and sort documents.
/// Returns 503 Service Unavailable when no reranker is configured.
#[instrument(skip(state, body), fields(docs_count, top_n))]
pub async fn handler(
    State(state): State<AppState>,
    Json(body): Json<RerankRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Request validation.
    if body.query.is_empty() {
        return Err(ApiError::InvalidBody(
            "query ne peut pas être vide".to_string(),
        ));
    }
    if body.documents.is_empty() {
        return Err(ApiError::InvalidBody(
            "documents ne peut pas être vide".to_string(),
        ));
    }

    let doc_count = body.documents.len();
    tracing::Span::current().record("docs_count", doc_count);
    if let Some(top_n) = body.top_n {
        tracing::Span::current().record("top_n", top_n);
    }

    // Check reranker availability — 503 when absent (feature not configured,
    // distinct from a transient backend error which would return 502).
    let reranker = state.reranker.as_ref().ok_or_else(|| {
        tracing::warn!("POST /v1/rerank : aucun reranker configuré");
        ApiError::ServiceUnavailable {
            message: "aucun reranker configuré dans AppState".to_string(),
        }
    })?;

    // Enforce batch cap — must be checked after resolving the reranker
    // (cap depends on the implementation: 20 for JinaOnnxReranker, usize::MAX for Noop).
    // Exceeding this threshold would cause unbounded pair-by-pair inference → ONNX OOM.
    let max_batch = reranker.max_batch_size();
    if doc_count > max_batch {
        return Err(ApiError::InvalidBody(format!(
            "too many documents: {} > max_batch_size {}",
            doc_count, max_batch
        )));
    }

    // Prepare candidates for the reranker: (doc_id, text).
    // The Reranker trait expects &[(String, String)].
    // Index is used as the identifier (format "idx_N").
    let candidates: Vec<(String, String)> = body
        .documents
        .iter()
        .enumerate()
        .map(|(i, doc)| (format!("idx_{}", i), doc.clone()))
        .collect();

    // The reranker is synchronous (ONNX CPU) — use spawn_blocking to avoid
    // blocking the runtime. spawn_blocking works on both current_thread (tests)
    // and multi_thread (production), unlike block_in_place which panics on
    // current_thread. The closure is 'static: Arc and data are cloned upfront.
    let query_owned = body.query.clone();
    let reranker_ref = std::sync::Arc::clone(reranker);
    let scores: Vec<f32> =
        tokio::task::spawn_blocking(move || reranker_ref.rerank(&query_owned, &candidates))
            .await
            .map_err(|join_err| {
                ApiError::Backend(LlmError::Custom {
                    message: format!("reranker task paniqué: {}", join_err),
                })
            })?
            .map_err(|e| {
                ApiError::Backend(LlmError::Custom {
                    message: format!("erreur reranker: {}", e),
                })
            })?;

    // Invariant: reranker.rerank() returns exactly as many scores as candidates.
    // Guaranteed by the Reranker trait contract (see gradatum-search/src/reranker.rs).
    // Any violation is an internal implementation bug — debug_assert catches it in
    // development without overhead in release builds.
    debug_assert_eq!(
        scores.len(),
        doc_count,
        "reranker.rerank() doit retourner exactement doc_count scores (got {}, expected {})",
        scores.len(),
        doc_count
    );

    // Build results with original index + score.
    let mut results: Vec<RerankResult> = scores
        .into_iter()
        .enumerate()
        .map(|(i, score)| RerankResult {
            index: i,
            document: body.documents[i].clone(),
            relevance_score: score,
        })
        .collect();

    // Sort by descending score.
    results.sort_by(|a, b| {
        b.relevance_score
            .partial_cmp(&a.relevance_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Apply top_n when specified.
    if let Some(top_n) = body.top_n {
        results.truncate(top_n);
    }

    tracing::debug!(
        returned = results.len(),
        total = doc_count,
        "rerank terminé"
    );

    Ok((StatusCode::OK, Json(RerankResponse { results })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rerank_result_sort_order() {
        let mut results = [
            RerankResult {
                index: 0,
                document: "doc0".to_string(),
                relevance_score: 0.5,
            },
            RerankResult {
                index: 1,
                document: "doc1".to_string(),
                relevance_score: 0.9,
            },
            RerankResult {
                index: 2,
                document: "doc2".to_string(),
                relevance_score: 0.1,
            },
        ];

        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        assert_eq!(results[0].index, 1, "score 0.9 doit être en premier");
        assert_eq!(results[1].index, 0, "score 0.5 doit être en deuxième");
        assert_eq!(results[2].index, 2, "score 0.1 doit être en dernier");
    }

    #[test]
    fn test_top_n_truncation() {
        let mut results: Vec<RerankResult> = (0..5)
            .map(|i| RerankResult {
                index: i,
                document: format!("doc{}", i),
                relevance_score: i as f32 * 0.1,
            })
            .collect();

        results.truncate(3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_top_n_larger_than_results_ok() {
        let mut results: Vec<RerankResult> = vec![RerankResult {
            index: 0,
            document: "doc0".to_string(),
            relevance_score: 0.8,
        }];
        // top_n=10 sur 1 résultat ne doit pas paniquer.
        results.truncate(10);
        assert_eq!(results.len(), 1);
    }
}
