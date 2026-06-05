//! Embedder local via fastembed (ONNX CPU).
//!
//! **Requiert la feature `fastembed-cpu`** — désactivée par défaut.
//! Voir `Cargo.toml` du crate pour les instructions d'activation.
//!
//! Utilise le modèle `bge-small-en-v1.5` (384 dimensions, anglais uniquement).
//! Les poids (~150 MB) sont téléchargés dans `~/.cache/fastembed/` au premier appel.
//!
//! ## Comportement en cas de Mutex poisonné
//!
//! `TextEmbedding::embed` prend `&self` en fastembed 4.6.0, donc le `Mutex` ne sert
//! qu'à satisfaire `Send`. En phase 1, un poison du Mutex est non-récupérable :
//! le process doit être redémarré. Le `.expect("mutex non-poisonné")` documente
//! ce choix intentionnel — à remplacer par une stratégie de recovery en phase 2.

use std::sync::Mutex;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::embedder_trait::{EmbedBackend, Embedder};
use crate::error::EmbedError;

/// Embedder CPU-only via fastembed ONNX.
///
/// `TextEmbedding` de fastembed n'est pas `Sync` (l'inférence ONNX utilise des
/// buffers internes non-thread-safe). On l'enveloppe dans un `Mutex` pour rendre
/// l'ensemble `Send + Sync` et utilisable dans des contextes multi-thread.
///
/// L'inférence reste synchrone (blocking). Pour les handlers Axum, utiliser
/// `tokio::task::spawn_blocking` en production (non requis pour Phase 1 CLI).
pub struct FastEmbedCpu {
    /// Modèle ONNX derrière un verrou pour Send+Sync.
    inner: Mutex<TextEmbedding>,
    /// Identifiant du modèle retourné par `embedder_id()`.
    embedder_id: String,
    /// Nombre de dimensions du modèle.
    dim: u16,
}

impl FastEmbedCpu {
    /// Construit un `FastEmbedCpu` avec le modèle par défaut `bge-small-en-v1.5` (384d, anglais).
    ///
    /// Télécharge les poids (~150 MB) dans `~/.cache/fastembed/` si absents.
    ///
    /// # Erreurs
    ///
    /// Retourne `EmbedError::Init` si le téléchargement ou l'initialisation ONNX échouent.
    pub fn try_default() -> Result<Self, EmbedError> {
        let inner = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(false),
        )
        .map_err(|e| EmbedError::Init(format!("fastembed init: {e}")))?;

        Ok(Self {
            inner: Mutex::new(inner),
            embedder_id: "bge-small-en-v1.5".into(),
            dim: 384,
        })
    }
}

#[async_trait]
impl Embedder for FastEmbedCpu {
    fn embedder_id(&self) -> &str {
        &self.embedder_id
    }

    fn dim(&self) -> u16 {
        self.dim
    }

    /// Calcule l'embedding d'un texte unique en déléguant vers `embed_batch`.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = self.embed_batch(&[text]).await?;
        // SAFETY: embed_batch garantit 1 vecteur si l'entrée contient 1 texte.
        Ok(out
            .pop()
            .expect("embed_batch a retourné exactement 1 vecteur pour 1 texte"))
    }

    /// Calcule les embeddings d'un lot de textes (synchrone, bloquant dans tokio).
    ///
    /// Le `Mutex` est acquis le temps de l'inférence et relâché immédiatement après.
    /// En phase 1, un poison du Mutex (panique dans un autre thread pendant l'inférence)
    /// est non-récupérable — le process doit être redémarré.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let texts_owned: Vec<String> = texts.iter().map(|s| (*s).to_string()).collect();

        // MUTEX POISON PHASE 1 : `.expect()` intentionnel — voir module doc.
        let guard = self
            .inner
            .lock()
            .expect("Mutex FastEmbedCpu non-poisonné : une panique dans un autre thread a corrompu l'état du modèle ONNX, redémarrer le process");

        guard
            .embed(texts_owned, None)
            .map_err(|e| EmbedError::Embed(format!("fastembed: {e}")))
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::FastembedCpu
    }
}
