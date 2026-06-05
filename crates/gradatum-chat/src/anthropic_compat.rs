//! Backend Anthropic — `POST /v1/messages`.
//!
//! Utilise l'API Anthropic Messages avec :
//! - Header `x-api-key: <key>`
//! - Header `anthropic-version: 2023-06-01`
//! - Prompt caching via `cache_control` sur le message système
//!
//! ## Prompt caching
//!
//! Le message système est annoté avec `"cache_control": {"type": "ephemeral"}`
//! pour activer le prompt caching Anthropic. Réduit la latence et les coûts
//! des appels répétés avec le même prompt système.
//!
//! ## Format body
//!
//! ```json
//! {
//!   "model": "claude-haiku-4-5",
//!   "max_tokens": 512,
//!   "system": [{"type": "text", "text": "...", "cache_control": {"type": "ephemeral"}}],
//!   "messages": [{"role": "user", "content": "..."}]
//! }
//! ```
//!
//! ## Format réponse
//!
//! `content[0].text` contient le texte de la réponse.
//!
//! Spec ref : plan P2.0b §"Step 5.6".

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::backend::{CuratorDecision, LlmBackend, LlmError};
use crate::openai_compat::{map_reqwest_err, parse_curator_decision};

const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Nombre maximum de tokens générés par le LLM si aucune valeur n'est configurée.
///
/// 1024 est aligné sur le gatekeeper legacy (`max_tokens: Some(1024)`)
/// et suffit largement pour une réponse JSON curator complète
/// (section + 5 tags + 5 wikilinks + duplicate_hint ≈ 150-300 tokens).
const DEFAULT_MAX_TOKENS: u32 = 1024;

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Backend Anthropic Messages API avec prompt caching.
///
/// # Exemple
///
/// ```rust,no_run
/// use gradatum_chat::anthropic_compat::AnthropicCompatBackend;
/// use secrecy::SecretString;
///
/// let backend = AnthropicCompatBackend::new(
///     SecretString::new("sk-ant-...".to_string().into()),
///     "claude-haiku-4-5".to_string(),
/// );
/// ```
pub struct AnthropicCompatBackend {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: SecretString,
    /// Nombre maximum de tokens dans la réponse du LLM.
    /// Défaut : [`DEFAULT_MAX_TOKENS`] (1024). Configurable via [`with_max_tokens`].
    ///
    /// [`with_max_tokens`]: AnthropicCompatBackend::with_max_tokens
    max_tokens: u32,
}

impl AnthropicCompatBackend {
    /// Crée un backend Anthropic avec l'API publique et timeout 30 secondes.
    ///
    /// `max_tokens` est initialisé à [`DEFAULT_MAX_TOKENS`] (1024).
    /// Utiliser [`with_max_tokens`] pour surcharger depuis la config TOML.
    ///
    /// [`with_max_tokens`]: AnthropicCompatBackend::with_max_tokens
    pub fn new(api_key: SecretString, model: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("construction reqwest Client avec timeout valide");
        Self {
            client,
            base_url: ANTHROPIC_API_BASE.to_string(),
            model,
            api_key,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Remplace l'URL de base (pour tests ou proxies).
    #[must_use]
    pub fn with_base_url(self, base_url: String) -> Self {
        Self { base_url, ..self }
    }

    /// Remplace le timeout (reconstruit le client interne).
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("construction reqwest Client avec timeout valide");
        Self { client, ..self }
    }

    /// Remplace le nombre maximum de tokens générés par le LLM.
    ///
    /// Câblé depuis `CuratorPipelineConfig.llm_review_max_tokens` via `from_config()`.
    /// La valeur TOML `[curator] llm_review_max_tokens = N` est propagée ici.
    #[must_use]
    pub fn with_max_tokens(self, n: u32) -> Self {
        Self {
            max_tokens: n,
            ..self
        }
    }
}

// --- DTOs Anthropic ---

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    /// Prompt système avec cache_control pour le prompt caching.
    system: Vec<AnthropicSystemBlock<'a>>,
    messages: Vec<AnthropicMessage<'a>>,
}

#[derive(Serialize)]
struct AnthropicSystemBlock<'a> {
    #[serde(rename = "type")]
    block_type: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl<'a>>,
}

#[derive(Serialize)]
struct AnthropicCacheControl<'a> {
    #[serde(rename = "type")]
    cache_type: &'a str,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    text: String,
}

#[async_trait]
impl LlmBackend for AnthropicCompatBackend {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn is_local(&self) -> bool {
        false
    }

    async fn classify(&self, system: &str, user: &str) -> Result<CuratorDecision, LlmError> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));

        let req = AnthropicRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            // Système avec cache_control ephemeral pour le prompt caching
            system: vec![AnthropicSystemBlock {
                block_type: "text",
                text: system,
                cache_control: Some(AnthropicCacheControl {
                    cache_type: "ephemeral",
                }),
            }],
            messages: vec![AnthropicMessage {
                role: "user",
                content: user,
            }],
        };

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", self.api_key.expose_secret())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&req)
            .send()
            .await
            .map_err(map_reqwest_err)?;

        let status = resp.status();
        if status == 401 || status == 403 {
            return Err(LlmError::AuthError);
        }
        if status == 429 {
            return Err(LlmError::RateLimit);
        }
        if status == 400 {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::BadRequest(body));
        }
        if status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::ServerError {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: AnthropicResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Parse(e.to_string()))?;

        let text = parsed
            .content
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Parse("empty content array in Anthropic response".into()))?
            .text;

        parse_curator_decision(&text)
    }
}
