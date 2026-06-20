//! Reranker module — cross-encoder abstraction for `vault_search` post-ranking.
//!
//! ## Architecture
//!
//! `Reranker` is a pluggable trait injected into `AppState.reranker`.
//! With the `onnx-reranker` feature: `JinaOnnxReranker`.
//! Without the feature or in tests: `NoopReranker` (preserves composite order).
//!
//! ## Performance
//!
//! `JinaOnnxReranker` is synchronous (ONNX Runtime CPU). The async handler uses
//! `tokio::task::spawn_blocking` to offload inference to a dedicated thread.
//! This is preferred over `block_in_place` because it is compatible with both
//! Tokio runtimes: `current_thread` (used by `#[tokio::test]`) and `multi_thread`
//! (production). The closure is `'static`: `Arc` and data are cloned upstream.
//! The `JoinError` returned by `.await` signals a panic in the blocking task.
//!
//! ## API ort 2.x — version locked
//!
//! Targets `ort = "=2.0.0-rc.9"` strictly. API used:
//! - `Session::builder()?.with_optimization_level(...)?.with_intra_threads(...)?.commit_from_file(...)?`
//! - `Tensor::from_array(array)?`
//! - `session.run(ort::inputs! { "input_ids" => ids, ... })?`
//! - `outputs[0].try_extract_tensor::<f32>()?` returns `(Shape, &[f32])` in 2.x

use gradatum_core::error::GradatumError;

// ── Trait public ──────────────────────────────────────────────────────────────

/// Cross-encoder reranker abstraction for `vault_search` post-ranking.
///
/// Allows swapping `JinaOnnxReranker` ↔ `NoopReranker` without changing the handler.
/// The handler holds an `Arc<dyn Reranker + Send + Sync>` in `AppState`.
pub trait Reranker: Send + Sync {
    /// Reranks a list of `(note_id, body_text)` pairs against a query.
    ///
    /// **Cardinality contract**: returns **exactly `candidates.len()` scores**,
    /// in the **same order** as `candidates` (index `i` → `scores[i]`).
    /// Any implementation violating this contract (`scores.len() ≠ candidates.len()`)
    /// is an internal bug — the handler asserts it via `debug_assert_eq!`.
    ///
    /// Scores are in `[0.0, 1.0]` — higher = more relevant.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Inference(String)` if ONNX inference fails.
    /// Never panics — clean fail-fast.
    fn rerank(
        &self,
        query: &str,
        candidates: &[(String, String)],
    ) -> Result<Vec<f32>, GradatumError>;

    /// Maximum number of candidates this reranker can process in a single pass.
    fn max_batch_size(&self) -> usize;

    /// Indicates whether the reranker requires the full `body_text` to produce a
    /// useful score. Allows the `vault_search` handler to avoid an N+1 `body_text`
    /// read when a `NoopReranker` is wired in.
    ///
    /// Default: `false` (`NoopReranker` — body not needed).
    /// `JinaOnnxReranker` must return `true`.
    fn requires_body(&self) -> bool {
        false
    }
}

// ── NoopReranker ──────────────────────────────────────────────────────────────

/// No-op reranker: returns monotonically decreasing placeholder scores that preserve order.
///
/// Used when the `onnx-reranker` feature is disabled or in tests.
/// Scores = `1.0 - i / (n + 1)` for `i = 0..n` → all in `(0.0, 1.0)`.
#[derive(Debug, Clone, Default)]
pub struct NoopReranker;

impl Reranker for NoopReranker {
    fn rerank(
        &self,
        _query: &str,
        candidates: &[(String, String)],
    ) -> Result<Vec<f32>, GradatumError> {
        let n = candidates.len();
        if n == 0 {
            return Ok(vec![]);
        }
        let denom = n as f32 + 1.0;
        let scores: Vec<f32> = (0..n).map(|i| 1.0 - (i as f32) / denom).collect();
        Ok(scores)
    }

    fn max_batch_size(&self) -> usize {
        usize::MAX
    }
}

// ── JinaOnnxReranker (feature `onnx-reranker`) ────────────────────────────────

#[cfg(feature = "onnx-reranker")]
mod onnx {
    use super::{GradatumError, Reranker};
    use ort::session::{Session, builder::GraphOptimizationLevel};
    use ort::value::Tensor;
    use std::path::Path;
    use std::sync::Arc;
    use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};

    /// Cross-encoder reranker backed by ONNX Runtime (stable 2.x API, version locked to rc.9).
    ///
    /// Default model: `cross-encoder/ms-marco-MiniLM-L-6-v2` (22.7 M parameters, 45 MB).
    /// Max tokens per `(query, doc)` pair: 512 (truncation `OnlySecond` configured at load
    /// time — raw `ids.truncate()` would corrupt the `[SEP]` token and must not be used).
    pub struct JinaOnnxReranker {
        session: Arc<Session>,
        tokenizer: Arc<Tokenizer>,
        /// Maximum token count preserved (truncation configured at load time — exposed for
        /// introspection and tests; not used inside `rerank()` because truncation is
        /// applied by the tokenizer).
        #[allow(dead_code)]
        max_length: usize,
    }

    impl JinaOnnxReranker {
        /// Loads the ONNX model and tokenizer from a local path.
        ///
        /// `model_path` must point to the `.onnx` file.
        /// The tokenizer is loaded from the same directory (`tokenizer.json`).
        ///
        /// Truncation is configured **at load time** via `TruncationParams { strategy: OnlySecond, ... }`:
        /// the query (first segment) is preserved; only the document (second segment) is truncated.
        /// This ensures the `[SEP]` separator token remains correct — a raw `Vec::truncate` on
        /// `ids` would corrupt the cross-encoder encoding.
        ///
        /// # Errors
        ///
        /// Returns `GradatumError::Inference` if the file is absent or invalid.
        pub fn from_file(model_path: &str) -> Result<Self, GradatumError> {
            let session = Session::builder()
                .map_err(|e| GradatumError::Inference(format!("ort session builder: {e}")))?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| GradatumError::Inference(format!("ort opt level: {e}")))?
                .with_intra_threads(2)
                .map_err(|e| GradatumError::Inference(format!("ort threads: {e}")))?
                .commit_from_file(model_path)
                .map_err(|e| GradatumError::Inference(format!("ort load model: {e}")))?;

            let model_dir = Path::new(model_path).parent().ok_or_else(|| {
                GradatumError::Inference("model_path sans répertoire parent".into())
            })?;
            let tokenizer_path = model_dir.join("tokenizer.json");

            let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
                .map_err(|e| GradatumError::Inference(format!("tokenizer load: {e}")))?;

            // Caveat A-rev2-3 : truncation OnlySecond (préserve query, tronque doc).
            let max_length: usize = 512;
            tokenizer
                .with_truncation(Some(TruncationParams {
                    max_length,
                    strategy: TruncationStrategy::OnlySecond,
                    stride: 0,
                    direction: TruncationDirection::Right,
                }))
                .map_err(|e| GradatumError::Inference(format!("tokenizer with_truncation: {e}")))?;

            Ok(Self {
                session: Arc::new(session),
                tokenizer: Arc::new(tokenizer),
                max_length,
            })
        }

        /// Tokenizes a `(query, doc)` pair using tokenizer 0.21 `EncodeInput::Dual`.
        ///
        /// `tokenizers::Tokenizer::encode((q, d), true)` accepts a `(&str, &str)` tuple
        /// that converts to `EncodeInput::Dual`. No standalone `encode_pair` exists in 0.21.
        ///
        /// Returns `(input_ids, attention_mask, token_type_ids)` — aliased as `TokenTriplet`
        /// to satisfy `clippy::type_complexity`.
        #[allow(clippy::type_complexity)]
        fn tokenize_pair(
            &self,
            query: &str,
            doc: &str,
        ) -> Result<(Vec<i64>, Vec<i64>, Vec<i64>), GradatumError> {
            let encoding = self
                .tokenizer
                .encode((query, doc), true)
                .map_err(|e| GradatumError::Inference(format!("tokenize: {e}")))?;

            let ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
            let mask: Vec<i64> = encoding
                .get_attention_mask()
                .iter()
                .map(|&x| x as i64)
                .collect();
            let type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|&x| x as i64).collect();

            Ok((ids, mask, type_ids))
        }
    }

    impl Reranker for JinaOnnxReranker {
        fn rerank(
            &self,
            query: &str,
            candidates: &[(String, String)],
        ) -> Result<Vec<f32>, GradatumError> {
            if candidates.is_empty() {
                return Ok(vec![]);
            }

            let mut scores = Vec::with_capacity(candidates.len());

            // Inférence paire-par-paire (batch_size=1) pour contrôle mémoire.
            //
            // Task 23 WONTFIX alpha.15 — tokenize-once via encode_batch dual-pair non faisable
            // sans re-tokenization de la query :
            //
            //   `tokenizers 0.21::Tokenizer::encode_batch(Vec<E>, bool)` accepte bien
            //   `Vec<(&str, &str)>` converti en `EncodeInput::Dual` via le From impl.
            //   Mais en interne, `encode_batch` appelle `encode` pour chaque paire en
            //   parallèle — ce qui déclenche `encode_single_sequence(query, 0, ...)` à
            //   chaque iteration (source : tokenizer/mod.rs:1262-1281). Aucun cache
            //   d'encoding intermédiaire n'est exposé par l'API publique 0.21.
            //
            //   Optimisation réelle (pré-tokenizer query + concat manuel avec Encoding)
            //   nécessiterait de manipuler les offsets + type_ids + padding à la main —
            //   complexité disproportionnée vs gain (<30ms sur N≤20 à ~5ms/pair).
            //
            // TODO α.16 — réévaluer si tokenizers 0.22 expose encode_batch avec cache
            // d'encoding intermédiaire, ou si F-08 (cross-encoder activé par défaut)
            // justifie un Encoding wrapper custom (Silver v0.3.0 scope).
            for (_note_id, body) in candidates {
                let (ids, mask, type_ids) = self.tokenize_pair(query, body)?;

                let seq_len = ids.len();
                // API ort rc.9 : Tensor::from_array attend `(shape: D, data: Vec<T>)`
                // (cf. impl IntoValueTensor for (D, Vec<T>) — pas Array2 directement).
                let shape: Vec<i64> = vec![1, seq_len as i64];

                let ids_tensor = Tensor::from_array((shape.clone(), ids))
                    .map_err(|e| GradatumError::Inference(format!("ids tensor: {e}")))?;
                let mask_tensor = Tensor::from_array((shape.clone(), mask))
                    .map_err(|e| GradatumError::Inference(format!("mask tensor: {e}")))?;
                let types_tensor = Tensor::from_array((shape.clone(), type_ids))
                    .map_err(|e| GradatumError::Inference(format!("type_ids tensor: {e}")))?;

                let outputs = self
                    .session
                    .run(
                        ort::inputs! {
                            "input_ids" => ids_tensor,
                            "attention_mask" => mask_tensor,
                            "token_type_ids" => types_tensor,
                        }
                        .map_err(|e| GradatumError::Inference(format!("inputs! macro: {e}")))?,
                    )
                    .map_err(|e| GradatumError::Inference(format!("ort run: {e}")))?;

                // API ort rc.9 : try_extract_raw_tensor (pas try_extract_tensor — renommé en rc.9).
                let (_shape, data) = outputs[0]
                    .try_extract_raw_tensor::<f32>()
                    .map_err(|e| GradatumError::Inference(format!("extract logits: {e}")))?;

                let logit = data
                    .first()
                    .copied()
                    .ok_or_else(|| GradatumError::Inference("logits vide".into()))?;
                // Sigmoid → [0.0, 1.0]
                let score = 1.0 / (1.0 + (-logit).exp());
                scores.push(score);
            }

            Ok(scores)
        }

        fn max_batch_size(&self) -> usize {
            20
        }

        fn requires_body(&self) -> bool {
            true
        }
    }
}

#[cfg(feature = "onnx-reranker")]
pub use onnx::JinaOnnxReranker;

#[cfg(test)]
mod tests {
    use super::*;

    // T14-1 : NoopReranker retourne l'ordre inchangé (scores décroissants).
    #[test]
    fn noop_reranker_preserves_order() {
        let reranker = NoopReranker;
        let candidates = vec![
            ("note_A".to_string(), "body A".to_string()),
            ("note_B".to_string(), "body B".to_string()),
        ];
        let scores = reranker.rerank("query", &candidates).unwrap();
        assert_eq!(scores.len(), 2, "scores doit avoir 2 éléments");
        assert!(scores[0] >= scores[1], "NoopReranker : ordre préservé");
    }

    // T14-2 : NoopReranker max_batch_size ≥ 20
    #[test]
    fn noop_reranker_max_batch_is_at_least_twenty() {
        let reranker = NoopReranker;
        assert!(reranker.max_batch_size() >= 20);
    }

    // T14-3 : scores retournés sont dans [0.0, 1.0]
    #[test]
    fn noop_reranker_scores_bounded() {
        let reranker = NoopReranker;
        let candidates: Vec<_> = (0..20)
            .map(|i| (format!("note_{i}"), format!("body {i}")))
            .collect();
        let scores = reranker.rerank("test query", &candidates).unwrap();
        for (i, &s) in scores.iter().enumerate() {
            assert!((0.0..=1.0).contains(&s), "score[{i}] = {s} hors [0.0, 1.0]");
        }
    }

    // T14-4 : batch vide → scores vides (pas de panic)
    #[test]
    fn reranker_empty_candidates_returns_empty() {
        let reranker = NoopReranker;
        let scores = reranker.rerank("query", &[]).unwrap();
        assert!(scores.is_empty(), "candidates vides → scores vides");
    }
}

// Tests d'intégration ONNX — feature gated + ignore (modèle local requis)
#[cfg(all(test, feature = "onnx-reranker"))]
mod onnx_tests {
    use super::*;

    // T14-5 : JinaOnnxReranker charge le modèle sans erreur
    #[test]
    #[ignore = "requiert RERANKER_ONNX_PATH=/path/to/model.onnx"]
    fn jina_onnx_reranker_loads_model() {
        let path = std::env::var("RERANKER_ONNX_PATH").expect("RERANKER_ONNX_PATH requis");
        let reranker = JinaOnnxReranker::from_file(&path).expect("chargement modèle ONNX");
        assert!(reranker.max_batch_size() > 0);
    }

    // T14-6 : JinaOnnxReranker — candidat pertinent score > non-pertinent
    #[test]
    #[ignore = "requiert RERANKER_ONNX_PATH"]
    fn jina_onnx_reranker_ranks_relevant_higher() {
        let path = std::env::var("RERANKER_ONNX_PATH").expect("RERANKER_ONNX_PATH requis");
        let reranker = JinaOnnxReranker::from_file(&path).unwrap();

        let query = "gradatum search architecture";
        let candidates = vec![
            (
                "note_A".to_string(),
                "gradatum uses BM25 FTS5 for search with semantic RRF".to_string(),
            ),
            (
                "note_B".to_string(),
                "today is a good day for a walk in the park".to_string(),
            ),
        ];
        let scores = reranker.rerank(query, &candidates).unwrap();
        assert_eq!(scores.len(), 2);
        assert!(
            scores[0] > scores[1],
            "note pertinente score={} > non-pertinente score={}",
            scores[0],
            scores[1]
        );
    }
}
