//! Tests `GradatumError::Inference`.
//!
//! Couvre :
//! 1. `inference_variant_displays_message` — Display formate "inference : <msg>".
//! 2. `inference_variant_propagates_via_question_mark` — utilisable avec `?`.
//! 3. `inference_variant_distinct_from_storage` — pattern matching distinct.

use gradatum_core::error::GradatumError;

/// Le variant `Inference(String)` doit s'afficher avec le préfixe `inference :`.
#[test]
fn inference_variant_displays_message() {
    let err = GradatumError::Inference("backend HTTP timeout".to_string());
    let msg = format!("{err}");
    assert!(
        msg.contains("inference"),
        "Display doit contenir 'inference', got: {msg}"
    );
    assert!(
        msg.contains("backend HTTP timeout"),
        "Display doit contenir le message source, got: {msg}"
    );
}

/// Le variant `Inference` doit être propageable via `?` dans une chaîne `Result<_, GradatumError>`.
#[test]
fn inference_variant_propagates_via_question_mark() {
    fn produce() -> Result<(), GradatumError> {
        Err(GradatumError::Inference("embed dim mismatch".to_string()))
    }
    fn consume() -> Result<(), GradatumError> {
        produce()?;
        Ok(())
    }
    let res = consume();
    assert!(matches!(res, Err(GradatumError::Inference(_))));
}

/// Pattern matching : `Inference` doit être distinct de `Storage` même si tous deux portent `String`.
#[test]
fn inference_variant_distinct_from_storage() {
    let inf = GradatumError::Inference("dim mismatch".into());
    let store = GradatumError::Storage("disk full".into());
    // Match : seulement Inference doit matcher.
    assert!(matches!(inf, GradatumError::Inference(_)));
    assert!(matches!(store, GradatumError::Storage(_)));
    assert!(!matches!(inf, GradatumError::Storage(_)));
    assert!(!matches!(store, GradatumError::Inference(_)));
}
