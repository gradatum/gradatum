//! Trait `Embedder` + types publics associés.

use async_trait::async_trait;

use crate::error::EmbedError;

/// Type de backend sous-jacent — utilisé pour le monitoring et le routage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbedBackend {
    /// Inférence locale via fastembed (ONNX CPU).
    FastembedCpu,
    /// Appel HTTP vers un backend OpenAI-compatible.
    Http,
    /// Noop — retourne des vecteurs nuls (tests / désactivation).
    Noop,
}

/// Trait principal pour tout embedder de texte.
///
/// Les implémentations doivent être `Send + Sync` pour être utilisées dans des
/// handlers Axum ou des workers tokio multi-thread.
///
/// # Contrat de dimension
///
/// `dim()` doit rester stable pour toute la durée de vie de l'instance.
/// Deux embedders de dimensions différentes ne peuvent pas être utilisés dans
/// la même index SQLite sans migration.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Identifiant du modèle d'embedding (ex : `"bge-small-en-v1.5"`, `"bge-m3"`).
    fn embedder_id(&self) -> &str;

    /// Nombre de dimensions des vecteurs produits.
    fn dim(&self) -> u16;

    /// Calcule l'embedding d'un texte unique.
    ///
    /// Délègue vers `embed_batch` avec un lot de taille 1 par défaut.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;

    /// Calcule les embeddings d'un lot de textes.
    ///
    /// L'ordre des vecteurs retournés correspond à l'ordre des textes en entrée.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// Type de backend sous-jacent (pour monitoring / logs / routage).
    fn backend_kind(&self) -> EmbedBackend;
}
