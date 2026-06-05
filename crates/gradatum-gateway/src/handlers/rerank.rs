//! Handler POST /v1/rerank — F-08 cross-encoder reranking.
//!
//! Utilise `Arc<dyn Reranker>` depuis `AppState.reranker` pour réordonner
//! une liste de passages par pertinence vis-à-vis d'une requête.
//!
//! Format de requête :
//! ```json
//! {
//!   "query": "ma question",
//!   "documents": ["doc1", "doc2", "doc3"],
//!   "top_n": 2
//! }
//! ```
//!
//! Format de réponse :
//! ```json
//! {
//!   "results": [
//!     { "index": 2, "document": "doc3", "relevance_score": 0.93 },
//!     { "index": 0, "document": "doc1", "relevance_score": 0.71 }
//!   ]
//! }
//! ```
//!
//! Codes d'erreur :
//! - 400 : requête invalide (query vide, documents vide)
//! - 503 : reranker non configuré
//! - 502 : erreur backend reranker

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::{commons::error::LlmError, error::ApiError, AppState};

/// Corps de la requête POST /v1/rerank.
#[derive(Debug, Deserialize)]
pub struct RerankRequest {
    /// Requête de recherche.
    pub query: String,
    /// Documents à réordonner.
    /// Chaque entrée est une chaîne de texte.
    pub documents: Vec<String>,
    /// Nombre maximum de résultats retournés (optionnel — tous si absent).
    #[serde(default)]
    pub top_n: Option<usize>,
}

/// Un résultat de reranking.
#[derive(Debug, Serialize)]
pub struct RerankResult {
    /// Indice dans le tableau `documents` original (0-indexé).
    pub index: usize,
    /// Contenu du document.
    pub document: String,
    /// Score de pertinence (0.0 à 1.0).
    pub relevance_score: f32,
}

/// Corps de la réponse POST /v1/rerank.
#[derive(Debug, Serialize)]
pub struct RerankResponse {
    pub results: Vec<RerankResult>,
}

/// Handler POST /v1/rerank
///
/// Utilise le reranker configuré dans AppState pour scorer et trier les documents.
/// Si aucun reranker n'est configuré → 503 Service Unavailable.
#[instrument(skip(state, body), fields(docs_count, top_n))]
pub async fn handler(
    State(state): State<AppState>,
    Json(body): Json<RerankRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Validation requête.
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

    // Vérification reranker disponible — 503 si absent (fonctionnalité non configurée,
    // distinct d'une erreur backend transiente qui retournerait 502).
    let reranker = state.reranker.as_ref().ok_or_else(|| {
        tracing::warn!("POST /v1/rerank : aucun reranker configuré");
        ApiError::ServiceUnavailable {
            message: "aucun reranker configuré dans AppState".to_string(),
        }
    })?;

    // Enforcement du cap batch — doit être vérifié après la résolution du reranker
    // (le cap dépend de l'implémentation : 20 pour JinaOnnxReranker, usize::MAX pour Noop).
    // Dépasser ce seuil entraînerait une inférence paire-par-paire illimitée → OOM ONNX.
    let max_batch = reranker.max_batch_size();
    if doc_count > max_batch {
        return Err(ApiError::InvalidBody(format!(
            "too many documents: {} > max_batch_size {}",
            doc_count, max_batch
        )));
    }

    // Préparer les candidats pour le reranker : (doc_id, texte).
    // Le Reranker trait attend &[(String, String)].
    // On utilise l'index comme identifiant (format "idx_N").
    let candidates: Vec<(String, String)> = body
        .documents
        .iter()
        .enumerate()
        .map(|(i, doc)| (format!("idx_{}", i), doc.clone()))
        .collect();

    // Le reranker est synchrone (ONNX CPU) — utiliser spawn_blocking pour ne pas
    // bloquer le runtime. spawn_blocking fonctionne sur current_thread (tests) ET
    // multi_thread (production), contrairement à block_in_place qui panique sur
    // current_thread. La closure est 'static : on clone l'Arc et les données au préalable.
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

    // Invariant : reranker.rerank() retourne exactement autant de scores que de candidats.
    // Garanti par le contrat du trait Reranker (cf. gradatum-search/src/reranker.rs).
    // Toute violation serait un bug d'implémentation interne — debug_assert pour le détecter
    // en développement sans overhead en release.
    debug_assert_eq!(
        scores.len(),
        doc_count,
        "reranker.rerank() doit retourner exactement doc_count scores (got {}, expected {})",
        scores.len(),
        doc_count
    );

    // Construire les résultats avec index original + score.
    let mut results: Vec<RerankResult> = scores
        .into_iter()
        .enumerate()
        .map(|(i, score)| RerankResult {
            index: i,
            document: body.documents[i].clone(),
            relevance_score: score,
        })
        .collect();

    // Tri par score décroissant.
    results.sort_by(|a, b| {
        b.relevance_score
            .partial_cmp(&a.relevance_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Appliquer top_n si spécifié.
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
