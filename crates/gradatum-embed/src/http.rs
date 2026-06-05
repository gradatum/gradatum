//! Embedder HTTP vers un backend OpenAI-compatible (`/v1/embeddings`).
//!
//! Compatible avec tout serveur OpenAI-compatible pour les embeddings
//! (LM Studio, vllm, llama.cpp server, bge-m3, etc.).
//!
//! ## Validation de dimension
//!
//! Si `dim > 0`, chaque réponse est validée : si le nombre de dimensions retourné
//! diffère de `self.dim`, `EmbedError::DimMismatch` est retourné.
//! Ceci protège contre les changements silencieux de modèle côté serveur.
//!
//! Si `dim == 0` (auto-detect), la dimension est inférée de la première réponse.
//! Note : en Phase 1, auto-detect n'est pas implémenté — utiliser `dim > 0`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::embedder_trait::{EmbedBackend, Embedder};
use crate::error::EmbedError;

// ── Structures de désérialisation de la réponse OpenAI embeddings ─────────────

/// Objet `data[i]` de la réponse `/v1/embeddings`.
#[derive(Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
    index: usize,
}

/// Corps complet de la réponse `/v1/embeddings`.
#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

// ── Requête ───────────────────────────────────────────────────────────────────

/// Corps de la requête POST `/v1/embeddings`.
#[derive(serde::Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

// ── HttpEmbedder ──────────────────────────────────────────────────────────────

/// Embedder HTTP vers un serveur OpenAI-compatible.
///
/// # Exemple d'URL
///
/// `http://your-embed-host:8432/v1/embeddings` — tout serveur OpenAI-compatible.
pub struct HttpEmbedder {
    client: reqwest::Client,
    /// URL complète du endpoint `/v1/embeddings`.
    endpoint: String,
    /// Nom du modèle transmis dans le corps de la requête.
    model: String,
    /// Timeout reqwest (reconstruit si changé via `with_timeout`).
    timeout: Duration,
    /// Identifiant de l'embedder (= nom du modèle).
    embedder_id: String,
    /// Nombre de dimensions attendu. 0 = non configuré (auto-detect non implémenté Phase 1).
    dim: u16,
}

impl HttpEmbedder {
    /// Crée un `HttpEmbedder` avec un timeout par défaut de 5 secondes.
    ///
    /// # Paramètres
    ///
    /// - `endpoint` : URL complète (ex : `"http://your-embed-host:8432/v1/embeddings"`)
    /// - `model` : nom du modèle (ex : `"bge-m3"`)
    /// - `dim` : dimensions attendues ; utiliser `0` pour désactiver la validation.
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>, dim: u16) -> Self {
        let endpoint = endpoint.into();
        let model = model.into();
        let embedder_id = model.clone();
        let timeout = Duration::from_secs(5);
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                // SAFETY: la config par défaut reqwest ne peut pas échouer.
                .expect("construction du client reqwest avec timeout par défaut"),
            endpoint,
            model,
            timeout,
            embedder_id,
            dim,
        }
    }

    /// Remplace le timeout (reconstruit le client reqwest interne).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            // SAFETY: idem `new` — config triviale.
            .expect("construction du client reqwest avec timeout personnalisé");
        self
    }

    /// Envoie la requête POST et désérialise la réponse.
    ///
    /// Réordonne les embeddings par `index` pour respecter l'ordre d'entrée
    /// (certains serveurs ne garantissent pas l'ordre de `data[]`).
    async fn call_endpoint(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let body = EmbedRequest {
            model: &self.model,
            input: texts.to_vec(),
        };

        let resp = self.client.post(&self.endpoint).json(&body).send().await?;

        let status = resp.status();
        if !status.is_success() {
            // Consomme le corps pour un message d'erreur plus lisible.
            let body_text = resp.text().await.unwrap_or_default();
            return Err(EmbedError::InvalidResponse(format!(
                "HTTP {status}: {body_text}"
            )));
        }

        let parsed: EmbeddingsResponse = resp
            .json()
            .await
            .map_err(|e| EmbedError::InvalidResponse(format!("désérialisation JSON: {e}")))?;

        if parsed.data.is_empty() {
            return Err(EmbedError::InvalidResponse(
                "data[] vide dans la réponse".into(),
            ));
        }

        // Tri par index pour garantir l'ordre même si le serveur réordonne.
        let mut items = parsed.data;
        items.sort_by_key(|item| item.index);

        // Validation de dimension si configurée.
        if self.dim > 0 {
            for item in &items {
                let got = item.embedding.len() as u16;
                if got != self.dim {
                    return Err(EmbedError::DimMismatch {
                        expected: self.dim,
                        got,
                    });
                }
            }
        }

        Ok(items.into_iter().map(|item| item.embedding).collect())
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    fn embedder_id(&self) -> &str {
        &self.embedder_id
    }

    fn dim(&self) -> u16 {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = self.embed_batch(&[text]).await?;
        // SAFETY: embed_batch retourne 1 vecteur pour 1 texte si la réponse est valide.
        Ok(out
            .pop()
            .expect("embed_batch a retourné exactement 1 vecteur pour 1 texte"))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.call_endpoint(texts).await
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Http
    }
}
