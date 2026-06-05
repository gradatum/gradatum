//! Erreurs du crate gradatum-embed.

use gradatum_core::error::GradatumError;
use thiserror::Error;

/// Conversion `EmbedError` → `GradatumError::Inference`.
///
/// Phase 2.x.2 alpha.11 patch.1 — F3.
///
/// L'orphan rule autorise cette `impl` côté `gradatum-embed` puisque
/// `gradatum-embed` dépend de `gradatum-core`.
///
/// Permet la propagation par `?` dans les handlers retournant `GradatumError`
/// et la conversion explicite `EmbedError -> GradatumError` en un point unique.
impl From<EmbedError> for GradatumError {
    fn from(err: EmbedError) -> Self {
        // On préserve le `Display` riche d'`EmbedError` (préfixe variant + message).
        GradatumError::Inference(err.to_string())
    }
}

/// Erreurs pouvant survenir lors de l'initialisation ou de l'utilisation d'un embedder.
#[derive(Debug, Error)]
pub enum EmbedError {
    /// Erreur lors de l'initialisation de l'embedder (chargement modèle, ONNX init…).
    #[error("init: {0}")]
    Init(String),

    /// Erreur lors du calcul d'embedding (inférence, batch échoué…).
    #[error("embed: {0}")]
    Embed(String),

    /// Erreur HTTP lors d'un appel vers un backend distant.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// Réponse HTTP inattendue ou malformée (JSON invalide, champs manquants…).
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Le nombre de dimensions retourné par le backend ne correspond pas à la valeur attendue.
    /// Protège contre les changements silencieux de modèle côté serveur.
    #[error("dim mismatch: expected {expected}, got {got}")]
    DimMismatch {
        /// Dimensions attendues (configurées par l'appelant).
        expected: u16,
        /// Dimensions reçues dans la réponse.
        got: u16,
    },
}
