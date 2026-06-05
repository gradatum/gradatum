//! Backend Google Gemini — `POST /v1beta/models/{model}:generateContent`.
//!
//! Utilise l'API Gemini avec :
//! - Header `x-goog-api-key: <key>`
//! - Body `{contents, systemInstruction, generationConfig}`
//!
//! ## Format body
//!
//! ```json
//! {
//!   "contents": [{"parts": [{"text": "..."}]}],
//!   "systemInstruction": {"parts": [{"text": "..."}]},
//!   "generationConfig": {"temperature": 0.0, "topP": 0.9, "maxOutputTokens": 64}
//! }
//! ```
//!
//! ## Format réponse
//!
//! `candidates[0].content.parts[0].text` contient le texte de la réponse.
//!
//! Spec ref : plan P2.0b §"Step 5.7".

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::backend::{CuratorDecision, LlmBackend, LlmError};
use crate::openai_compat::{map_reqwest_err, parse_curator_decision};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com";

/// Backend Google Gemini via `generateContent`.
///
/// # Exemple
///
/// ```rust,no_run
/// use gradatum_chat::gemini_compat::GeminiCompatBackend;
/// use secrecy::SecretString;
///
/// let backend = GeminiCompatBackend::new(
///     SecretString::new("AIza...".to_string().into()),
///     "gemini-1.5-flash".to_string(),
/// );
/// ```
pub struct GeminiCompatBackend {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: SecretString,
}

impl GeminiCompatBackend {
    /// Crée un backend Gemini avec l'API publique et timeout 30 secondes.
    pub fn new(api_key: SecretString, model: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("construction reqwest Client avec timeout valide");
        Self {
            client,
            base_url: GEMINI_API_BASE.to_string(),
            model,
            api_key,
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
}

// --- DTOs Gemini ---

#[derive(Serialize)]
struct GeminiRequest<'a> {
    contents: Vec<GeminiContent<'a>>,
    #[serde(rename = "systemInstruction")]
    system_instruction: GeminiSystemInstruction<'a>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
struct GeminiSystemInstruction<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    #[serde(rename = "topP")]
    top_p: f32,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiCandidateContent,
}

#[derive(Deserialize)]
struct GeminiCandidateContent {
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    text: String,
}

#[async_trait]
impl LlmBackend for GeminiCompatBackend {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn is_local(&self) -> bool {
        false
    }

    async fn classify(&self, system: &str, user: &str) -> Result<CuratorDecision, LlmError> {
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            self.model
        );

        let req = GeminiRequest {
            contents: vec![GeminiContent {
                parts: vec![GeminiPart { text: user }],
            }],
            system_instruction: GeminiSystemInstruction {
                parts: vec![GeminiPart { text: system }],
            },
            generation_config: GeminiGenerationConfig {
                temperature: 0.0,
                top_p: 0.9,
                max_output_tokens: 64,
            },
        };

        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", self.api_key.expose_secret())
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

        let parsed: GeminiResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Parse(e.to_string()))?;

        let text = parsed
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Parse("no candidates in Gemini response".into()))?
            .content
            .parts
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Parse("no parts in Gemini candidate".into()))?
            .text;

        parse_curator_decision(&text)
    }
}
