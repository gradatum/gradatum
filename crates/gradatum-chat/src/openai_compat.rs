//! Backend OpenAI-compatible — `POST /v1/chat/completions`.
//!
//! Compatible avec :
//! - OpenAI API
//! - llama.cpp server / OpenRouter / any OpenAI-compatible host
//! - Tout serveur exposant l'API chat/completions v1
//!
//! ## Authentification
//!
//! Bearer token via header `Authorization: Bearer <key>`. Laissé vide si la
//! clé est une chaîne vide (mode sans auth pour endpoints LAN internes).
//!
//! Spec ref : plan P2.0b §"Step 5.4".

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::backend::{CuratorDecision, LlmBackend, LlmError};

/// Timeout par défaut pour les requêtes LLM (classification curator courte).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Nombre maximum de tokens générés par le LLM si aucune valeur n'est configurée.
///
/// 1024 est aligné sur le gatekeeper legacy (`max_tokens: Some(1024)`)
/// et suffit largement pour une réponse JSON curator complète
/// (section + 5 tags + 5 wikilinks + duplicate_hint ≈ 150-300 tokens).
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Backend HTTP OpenAI-compatible.
///
/// # Exemple
///
/// ```rust,no_run
/// use gradatum_chat::openai_compat::OpenAiCompatBackend;
/// use secrecy::SecretString;
///
/// let backend = OpenAiCompatBackend::new(
///     "http://your-llm-host:8435".to_string(),
///     "qwen3-4b".to_string(),
///     SecretString::new("my-bearer-token".to_string().into()),
/// );
/// ```
pub struct OpenAiCompatBackend {
    client: Client,
    base_url: String,
    model: String,
    api_key: SecretString,
    /// Nombre maximum de tokens dans la réponse du LLM.
    /// Défaut : [`DEFAULT_MAX_TOKENS`] (1024). Configurable via [`with_max_tokens`].
    ///
    /// [`with_max_tokens`]: OpenAiCompatBackend::with_max_tokens
    max_tokens: u32,
}

impl OpenAiCompatBackend {
    /// Crée un backend avec timeout par défaut de 30 secondes.
    ///
    /// `max_tokens` est initialisé à [`DEFAULT_MAX_TOKENS`] (1024).
    /// Utiliser [`with_max_tokens`] pour surcharger depuis la config TOML.
    ///
    /// [`with_max_tokens`]: OpenAiCompatBackend::with_max_tokens
    pub fn new(base_url: String, model: String, api_key: SecretString) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("construction reqwest Client avec timeout valide");
        Self {
            client,
            base_url,
            model,
            api_key,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Remplace le timeout (reconstruit le client interne).
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        let client = Client::builder()
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

// --- DTOs reqwest ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    top_p: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatRespMsg,
}

#[derive(Deserialize)]
struct ChatRespMsg {
    content: String,
}

#[async_trait]
impl LlmBackend for OpenAiCompatBackend {
    fn name(&self) -> &'static str {
        "openai_compat"
    }

    fn is_local(&self) -> bool {
        // Considers loopback and RFC 1918 private ranges as local.
        crate::is_local_url(&self.base_url)
    }

    async fn classify(&self, system: &str, user: &str) -> Result<CuratorDecision, LlmError> {
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let req = ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: user,
                },
            ],
            temperature: 0.0,
            top_p: 0.9,
            max_tokens: self.max_tokens,
        };

        let mut builder = self.client.post(&url).json(&req);
        let key = self.api_key.expose_secret();
        if !key.is_empty() {
            builder = builder.bearer_auth(key);
        }

        let resp = builder.send().await.map_err(map_reqwest_err)?;

        map_status_code(&resp)?;

        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Parse(e.to_string()))?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Parse("no choices in response".into()))?
            .message
            .content;

        parse_curator_decision(&content)
    }
}

// --- Helpers partagés ---

/// Convertit une erreur reqwest en `LlmError`.
pub(crate) fn map_reqwest_err(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::Timeout
    } else if e.is_connect() {
        LlmError::ConnectionRefused
    } else if let Some(status) = e.status() {
        LlmError::ServerError {
            status: status.as_u16(),
            body: e.to_string(),
        }
    } else {
        LlmError::Transport(e.to_string())
    }
}

/// Vérifie le code HTTP et retourne l'erreur adéquate.
pub(crate) fn map_status_code(resp: &reqwest::Response) -> Result<(), LlmError> {
    let status = resp.status();
    if status == 429 {
        return Err(LlmError::RateLimit);
    }
    if status == 401 || status == 403 {
        return Err(LlmError::AuthError);
    }
    if status == 400 {
        return Err(LlmError::BadRequest(status.to_string()));
    }
    if status.is_server_error() {
        return Err(LlmError::ServerError {
            status: status.as_u16(),
            body: String::new(),
        });
    }
    Ok(())
}

/// Parse le contenu texte de la réponse LLM en `CuratorDecision`.
///
/// Tente un parse JSON direct, puis extrait le premier bloc `{...}` si échec
/// (gestion du préambule LLM "Here is the JSON:").
pub(crate) fn parse_curator_decision(content: &str) -> Result<CuratorDecision, LlmError> {
    // Tentative directe
    if let Ok(d) = serde_json::from_str::<CuratorDecision>(content.trim()) {
        return Ok(d);
    }

    // Extraction du premier bloc JSON {...}
    // Cherche la première accolade ouvrante et la dernière fermante correspondante
    if let Some(start) = content.find('{') {
        if let Some(end) = content.rfind('}') {
            if end > start {
                let json_slice = &content[start..=end];
                return serde_json::from_str::<CuratorDecision>(json_slice)
                    .map_err(|e| LlmError::Parse(format!("json extract failed: {e}")));
            }
        }
    }

    Err(LlmError::Parse(format!(
        "no valid JSON in response: {content:?}"
    )))
}
