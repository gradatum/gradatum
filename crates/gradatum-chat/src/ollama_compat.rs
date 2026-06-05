//! Backend Ollama natif — `POST /api/chat`.
//!
//! Utilise l'API Ollama native (pas le mode OpenAI-compat `/v1/`).
//! Endpoint par défaut : `http://127.0.0.1:11434`.
//!
//! ## Format body
//!
//! ```json
//! {
//!   "model": "qwen3:4b",
//!   "messages": [{"role": "system", "content": "..."}, {"role": "user", "content": "..."}],
//!   "stream": false,
//!   "options": {"temperature": 0.0, "num_predict": 64}
//! }
//! ```
//!
//! ## Format réponse
//!
//! `response.message.content` contient le texte de la réponse.
//!
//! Spec ref : plan P2.0b §"Step 5.5".

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backend::{CuratorDecision, LlmBackend, LlmError};
use crate::openai_compat::{map_reqwest_err, map_status_code, parse_curator_decision};

const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Backend Ollama — appelle directement l'API `/api/chat`.
///
/// # Exemple
///
/// ```rust,no_run
/// use gradatum_chat::ollama_compat::OllamaCompatBackend;
///
/// let backend = OllamaCompatBackend::new(
///     "http://127.0.0.1:11434".to_string(),
///     "qwen3:4b".to_string(),
/// );
/// ```
pub struct OllamaCompatBackend {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

impl OllamaCompatBackend {
    /// Crée un backend Ollama avec timeout par défaut de 30 secondes.
    pub fn new(base_url: String, model: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("construction reqwest Client avec timeout valide");
        Self {
            client,
            base_url,
            model,
        }
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
}

// --- DTOs Ollama ---

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaMessage<'a>>,
    stream: bool,
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct OllamaOptions {
    temperature: f32,
    num_predict: u32,
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: OllamaResponseMessage,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

#[async_trait]
impl LlmBackend for OllamaCompatBackend {
    fn name(&self) -> &'static str {
        "ollama_compat"
    }

    fn is_local(&self) -> bool {
        // Considers loopback and RFC 1918 private ranges as local.
        crate::is_local_url(&self.base_url)
    }

    async fn classify(&self, system: &str, user: &str) -> Result<CuratorDecision, LlmError> {
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

        let req = OllamaRequest {
            model: &self.model,
            messages: vec![
                OllamaMessage {
                    role: "system",
                    content: system,
                },
                OllamaMessage {
                    role: "user",
                    content: user,
                },
            ],
            stream: false,
            options: OllamaOptions {
                temperature: 0.0,
                num_predict: 64,
            },
        };

        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(map_reqwest_err)?;

        map_status_code(&resp)?;

        let parsed: OllamaResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Parse(e.to_string()))?;

        parse_curator_decision(&parsed.message.content)
    }
}
