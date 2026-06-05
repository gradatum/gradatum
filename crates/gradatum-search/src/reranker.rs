//! Module Reranker — abstraction cross-encoder pour le post-ranking vault_search.
//!
//! ## Architecture
//!
//! `Reranker` est un trait pluggable injecté dans `AppState.reranker`.
//! En production (feature `onnx-reranker`) : `JinaOnnxReranker`.
//! En test / sans feature : `NoopReranker` (préserve l'ordre composite).
//!
//! ## Performance
//!
//! `JinaOnnxReranker` est synchrone (ONNX Runtime CPU). Le handler async utilise
//! `tokio::task::spawn_blocking` pour déléguer l'inférence à un thread dédié.
//! Ce choix est préféré à `block_in_place` car il est compatible avec les deux
//! runtimes Tokio : `current_thread` (utilisé par `#[tokio::test]`) et `multi_thread`
//! (production). La closure est `'static` : `Arc` et données clonés en amont.
//! Le `JoinError` retourné par `.await` signale une panique dans la tâche bloquante.
//!
//! ## API ort 2.x — version locked
//!
//! Cette implémentation cible `ort = "=2.0.0-rc.9"` STRICTEMENT (cf. spec rev2 §10.1
//! politique maintenance ort RC). API utilisée :
//! - `Session::builder()?.with_optimization_level(...)?.with_intra_threads(...)?.commit_from_file(...)?`
//! - `Tensor::from_array(array)?`
//! - `session.run(ort::inputs! { "input_ids" => ids, ... })?`
//! - `outputs[0].try_extract_tensor::<f32>()?` retourne `(Shape, &[f32])` en 2.x

use gradatum_core::error::GradatumError;

// ── Trait public ──────────────────────────────────────────────────────────────

/// Abstraction cross-encoder reranker pour le post-ranking vault_search.
///
/// Permet de swapper `JinaOnnxReranker` ↔ `NoopReranker` sans changer le handler.
/// Le handler tient un `Arc<dyn Reranker + Send + Sync>` dans `AppState`.
pub trait Reranker: Send + Sync {
    /// Reranke une liste de `(note_id, body_text)` par rapport à une query.
    ///
    /// **Contrat de cardinalité** : retourne **exactement `candidates.len()` scores**,
    /// dans le **même ordre** que `candidates` (index i → scores[i]).
    /// Toute implémentation qui violerait ce contrat (scores.len() ≠ candidates.len())
    /// constitue un bug interne — le handler l'asserte via `debug_assert_eq!`.
    ///
    /// Score en `[0.0, 1.0]` — plus élevé = plus pertinent.
    ///
    /// # Erreurs
    ///
    /// Retourne `GradatumError::Inference(String)` si l'inférence ONNX échoue.
    /// Pas de panic — fail fast propre.
    fn rerank(
        &self,
        query: &str,
        candidates: &[(String, String)],
    ) -> Result<Vec<f32>, GradatumError>;

    /// Nombre maximal de candidats que ce reranker peut traiter en une passe.
    fn max_batch_size(&self) -> usize;

    /// Indique si le reranker exige le `body_text` complet pour produire un score
    /// utile. Permet au handler `vault_search` d'éviter une N+1 lecture body_text
    /// quand un `NoopReranker` est câblé.
    ///
    /// Default : `false` (NoopReranker — pas besoin du body).
    /// `JinaOnnxReranker` doit retourner `true`.
    fn requires_body(&self) -> bool {
        false
    }
}

// ── NoopReranker ──────────────────────────────────────────────────────────────

/// Reranker no-op : retourne des scores fictifs décroissants préservant l'ordre.
///
/// Utilisé quand la feature `onnx-reranker` est désactivée ou en test.
/// Scores = `1.0 - i / (n + 1)` pour `i = 0..n` → tous dans `(0.0, 1.0)`.
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
    use ort::session::{builder::GraphOptimizationLevel, Session};
    use ort::value::Tensor;
    use std::path::Path;
    use std::sync::Arc;
    use tokenizers::{Tokenizer, TruncationDirection, TruncationParams, TruncationStrategy};

    /// Cross-encoder reranker basé sur ONNX Runtime (API 2.x stable, version locked rc.9).
    ///
    /// Modèle par défaut : `cross-encoder/ms-marco-MiniLM-L-6-v2` (22.7M, 45 MB).
    /// Max tokens par paire `(query, doc)` : 512 (truncation OnlySecond configurée
    /// au load — caveat A-rev2-3, pas de `ids.truncate()` brut qui corromprait `[SEP]`).
    pub struct JinaOnnxReranker {
        session: Arc<Session>,
        tokenizer: Arc<Tokenizer>,
        /// Max tokens préservés (truncation configurée au load — exposé pour
        /// introspection / debug et test ; non utilisé dans rerank() car la
        /// truncation est appliquée par le tokenizer.
        #[allow(dead_code)]
        max_length: usize,
    }

    impl JinaOnnxReranker {
        /// Charge le modèle ONNX et le tokenizer depuis le path local.
        ///
        /// `model_path` doit pointer vers le fichier `.onnx`.
        /// Le tokenizer est chargé depuis le même répertoire (`tokenizer.json`).
        ///
        /// # Caveat A-rev2-3
        ///
        /// La truncation est configurée AU LOAD via `TruncationParams { strategy: OnlySecond, ... }` :
        /// la query (premier segment) est préservée, seul le document (second segment) est tronqué.
        /// Cela garantit que le séparateur `[SEP]` reste correct — un `Vec::truncate` brut sur
        /// les `ids` corromprait l'encodage cross-encoder.
        ///
        /// # Erreurs
        ///
        /// Retourne `GradatumError::Inference` si le fichier est absent ou invalide.
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

        /// Tokenize une paire `(query, doc)` via tokenizer 0.21 EncodeInput::Dual.
        ///
        /// `tokenizers::Tokenizer::encode((q, d), true)` accepte un tuple `(&str, &str)`
        /// qui se convertit en `EncodeInput::Dual`. Pas de `encode_pair` autonome en 0.21.
        ///
        /// Retourne `(input_ids, attention_mask, token_type_ids)` — alias `TokenTriplet`
        /// pour clippy::type_complexity.
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
