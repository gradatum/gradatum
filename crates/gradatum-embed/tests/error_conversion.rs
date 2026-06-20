//! Tests conversion `EmbedError` → `GradatumError::Inference`.
//!
//! L'orphan rule autorise `impl From<EmbedError> for GradatumError` ici puisque
//! `gradatum-embed` dépend de `gradatum-core` (et non l'inverse).

use gradatum_core::error::GradatumError;
use gradatum_embed::error::EmbedError;

/// `EmbedError::Embed` se convertit en `GradatumError::Inference` via `From`.
#[test]
fn embed_error_embed_converts_to_inference() {
    let src = EmbedError::Embed("backend HTTP 503".to_string());
    let g: GradatumError = src.into();
    assert!(
        matches!(g, GradatumError::Inference(_)),
        "EmbedError::Embed doit se convertir en GradatumError::Inference, got: {g:?}"
    );
}

/// `EmbedError::Init` se convertit en `GradatumError::Inference`.
#[test]
fn embed_error_init_converts_to_inference() {
    let src = EmbedError::Init("ONNX model not found".to_string());
    let g: GradatumError = src.into();
    assert!(matches!(g, GradatumError::Inference(_)));
}

/// `EmbedError::DimMismatch` se convertit en `GradatumError::Inference` avec message lisible.
#[test]
fn embed_error_dim_mismatch_converts_to_inference_with_msg() {
    let src = EmbedError::DimMismatch {
        expected: 384,
        got: 768,
    };
    let g: GradatumError = src.into();
    let msg = format!("{g}");
    assert!(matches!(g, GradatumError::Inference(_)));
    assert!(
        msg.contains("384") && msg.contains("768"),
        "le message doit contenir les dimensions, got: {msg}"
    );
}

/// Le `?` operator doit fonctionner pour propager `EmbedError` dans une fn renvoyant `GradatumError`.
#[test]
fn embed_error_propagates_via_question_mark() {
    fn produce() -> Result<Vec<f32>, EmbedError> {
        Err(EmbedError::Embed("simulated failure".into()))
    }
    fn consume() -> Result<Vec<f32>, GradatumError> {
        let v = produce()?;
        Ok(v)
    }
    let res = consume();
    assert!(matches!(res, Err(GradatumError::Inference(_))));
}
