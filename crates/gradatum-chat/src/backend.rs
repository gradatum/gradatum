//! Trait `LlmBackend` + types partagés pour les backends P2.0b.
//!
//! Différent du trait [`crate::chat_trait::Chat`] (Phase 1) qui opère sur des `Note`
//! et retourne un `CuratorVerdict`. `LlmBackend` est plus bas niveau : il prend un
//! prompt système + utilisateur et retourne une `CuratorDecision` JSON structurée.
//!
//! ## Design
//!
//! Les 5 backends implémentent tous ce trait :
//! - `HeuristicBackend` : offline, dispatch interne regex
//! - `OpenAiCompatBackend` : OpenAI / llama.cpp / OpenRouter / any OpenAI-compatible host
//! - `OllamaCompatBackend` : Ollama natif `/api/chat`
//! - `AnthropicCompatBackend` : Anthropic avec prompt caching
//! - `GeminiCompatBackend` : Google Gemini `/v1beta/models/{model}:generateContent`
//!
//! ## Circuit breaker
//!
//! [`crate::circuit_breaker_llm::CircuitBreaker`] wrappant `LlmBackend` avec
//! backoff exponentiel et fallback transparent vers `HeuristicBackend`.
//!
//! Spec ref : plan P2.0b §"Tasks 5-9".

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Erreurs retournées par un `LlmBackend`.
#[derive(Debug, Error)]
pub enum LlmError {
    /// Timeout réseau.
    #[error("timeout")]
    Timeout,

    /// Connexion refusée.
    #[error("connection refused")]
    ConnectionRefused,

    /// Erreur DNS.
    #[error("dns failure: {0}")]
    DnsFailure(String),

    /// Rate limit (HTTP 429).
    #[error("rate limit (429)")]
    RateLimit,

    /// Erreur serveur (5xx).
    #[error("server error ({status}): {body}")]
    ServerError {
        /// Code HTTP de la réponse serveur.
        status: u16,
        /// Corps de la réponse d'erreur (peut être vide).
        body: String,
    },

    /// Erreur d'authentification (401/403) — ne compte PAS pour le circuit.
    #[error("auth error (401/403)")]
    AuthError,

    /// Requête invalide (400) — ne compte PAS pour le circuit.
    #[error("bad request (400): {0}")]
    BadRequest(String),

    /// Erreur de parsing de la réponse LLM.
    #[error("parse: {0}")]
    Parse(String),

    /// Erreur de transport HTTP générique.
    #[error("transport: {0}")]
    Transport(String),
}

impl LlmError {
    /// Indique si cette erreur doit être comptabilisée par le circuit breaker.
    ///
    /// - `AuthError` et `BadRequest` sont des erreurs de configuration permanentes
    ///   (ne se corrigent pas avec le temps) → ne comptent pas.
    /// - Timeout, connexion, rate limit, erreur serveur comptent.
    pub fn counts_for_circuit(&self) -> bool {
        !matches!(self, LlmError::AuthError | LlmError::BadRequest(_))
    }
}

/// Décision structurée retournée par le LLM après classification d'une note.
///
/// Correspond à la sortie JSON attendue par le prompt curator-classifier-v1.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorDecision {
    /// Section canonique parmi les 10 sections gradatum.
    pub section: String,
    /// Tags extraits (2-5, kebab-case).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Wikilinks détectés dans le body (`[[NoteTitle]]`).
    #[serde(default)]
    pub wikilinks: Vec<String>,
    /// Titre d'un doublon potentiel, ou `null`.
    #[serde(default)]
    pub duplicate_hint: Option<String>,
}

/// Trait backend-agnostique pour les backends LLM curator P2.0b.
///
/// Toutes les implémentations sont `Send + Sync + 'static` pour pouvoir
/// être wrappées dans `Arc<dyn LlmBackend>`.
#[async_trait]
pub trait LlmBackend: Send + Sync + 'static {
    /// Nom court identifiant le backend (pour les logs/métriques).
    fn name(&self) -> &'static str;

    /// `true` si le backend tourne localement (pas d'appel réseau externe).
    fn is_local(&self) -> bool;

    /// Classifie une note via un prompt système + utilisateur.
    ///
    /// - `system` : prompt système (curator-classifier-v1 system message)
    /// - `user`   : contenu utilisateur formaté ("Classify this note.\nTitle: ...\nBody: ...")
    ///
    /// # Erreurs
    ///
    /// Retourne `LlmError` en cas d'échec. L'appelant (circuit breaker) gère
    /// le fallback vers `HeuristicBackend`.
    async fn classify(&self, system: &str, user: &str) -> Result<CuratorDecision, LlmError>;
}
