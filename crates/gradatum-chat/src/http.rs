//! Backend HTTP OpenAI-compatible — `/v1/chat/completions`.
//!
//! Compatible avec gateway-v2 (:8435) et tout backend OpenAI-compat (`/v1/chat/completions`).
//!
//! ## Parsing robuste
//!
//! Le LLM peut préfixer sa réponse JSON d'un préambule texte ("Here is the JSON:\n{…}").
//! L'implémentation tente d'abord un parse direct, puis effectue une extraction regex
//! du premier bloc `{…}` si le parse direct échoue.
//!
//! Spec ref : plan T07 sous-tâche T07b + D-perf-3.

use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::sync::OnceLock;

use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
use crate::error::ChatError;

// --- Defaults ---

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_TOKENS: u32 = 256;
const DEFAULT_MODEL: &str = "qwen3.6-35b-a3b-q4-k-xl";

/// Extrait le premier bloc JSON `{…}` d'une string (gestion préambule LLM).
fn re_json_block() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\{[^{}]*\}").expect("re_json_block est un pattern littéral valide")
    })
}

/// Prompt système injecté pour forcer une réponse JSON structurée.
const SYSTEM_PROMPT: &str = "You are a curator. Classify the note. \
Reply ONLY in JSON: \
{\"status\":\"live|pending-review|staging|garbage\",\
\"confidence\":0.0-1.0,\
\"reason\":\"...\"}";

// --- DTO OpenAI response ---

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    content: String,
}

/// DTO interne — verdict JSON attendu du LLM.
#[derive(Debug, Deserialize)]
struct LlmVerdict {
    status: String,
    confidence: f32,
    reason: String,
}

/// Convertit un statut string (LLM output) vers `NoteStatus`.
fn parse_status(s: &str) -> Result<NoteStatus, ChatError> {
    match s.trim().to_lowercase().as_str() {
        "live" => Ok(NoteStatus::Live),
        "pending-review" | "pending_review" => Ok(NoteStatus::PendingReview),
        "staging" => Ok(NoteStatus::Staging),
        "garbage" => Ok(NoteStatus::Garbage),
        "draft" => Ok(NoteStatus::Draft),
        "deprecated" => Ok(NoteStatus::Deprecated),
        other => Err(ChatError::InvalidResponse(format!(
            "statut LLM inconnu: {other:?} (attendu: live|pending-review|staging|garbage)"
        ))),
    }
}

/// Parse le contenu texte de la réponse LLM en `LlmVerdict`.
///
/// Tente un parse JSON direct, puis extrait le premier bloc `{…}` si échec.
fn extract_verdict(content: &str) -> Result<LlmVerdict, ChatError> {
    // Tentative directe
    if let Ok(v) = serde_json::from_str::<LlmVerdict>(content.trim()) {
        return Ok(v);
    }

    // Extraction regex du premier bloc JSON
    let caps = re_json_block().find(content).ok_or_else(|| {
        ChatError::ParseFailure(format!("aucun bloc JSON trouvé dans: {content:?}"))
    })?;

    serde_json::from_str::<LlmVerdict>(caps.as_str()).map_err(|e| {
        ChatError::ParseFailure(format!(
            "JSON extrait invalide: {e} — contenu: {:?}",
            caps.as_str()
        ))
    })
}

/// Backend HTTP OpenAI-compatible.
///
/// # Exemple
///
/// ```rust,no_run
/// use gradatum_chat::http::HttpChat;
///
/// let chat = HttpChat::new("http://localhost:8435/v1/chat/completions", "my-model")
///     .with_timeout(std::time::Duration::from_secs(15))
///     .with_max_tokens(128);
/// ```
pub struct HttpChat {
    client: reqwest::Client,
    /// URL complète de l'endpoint `/v1/chat/completions`.
    endpoint: String,
    /// Identifiant du modèle passé dans le champ `model` du body.
    model: String,
    /// Timeout configuré — conservé pour reconstruire le client via `with_timeout`.
    // NOTE: intentionnellement stocké même si non relu directement (sert à with_timeout).
    #[allow(dead_code)]
    timeout: Duration,
    /// Nombre maximum de tokens dans la réponse du LLM.
    max_tokens: u32,
}

impl HttpChat {
    /// Crée un `HttpChat` avec les valeurs par défaut.
    ///
    /// - timeout : 30 secondes
    /// - max_tokens : 256
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        let timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("la configuration reqwest par défaut est toujours valide");
        Self {
            client,
            endpoint: endpoint.into(),
            model: model.into(),
            timeout,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Remplace le timeout (reconstruit le client interne).
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("timeout Duration est toujours valide pour reqwest");
        Self {
            client,
            timeout,
            ..self
        }
    }

    /// Remplace le nombre maximum de tokens.
    #[must_use]
    pub fn with_max_tokens(self, n: u32) -> Self {
        Self {
            max_tokens: n,
            ..self
        }
    }
}

impl Default for HttpChat {
    fn default() -> Self {
        Self::new("http://localhost:8435/v1/chat/completions", DEFAULT_MODEL)
    }
}

#[async_trait]
impl Chat for HttpChat {
    async fn classify_curator(
        &self,
        note: &Note,
        _context: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        // Sérialise la note comme contenu utilisateur (JSON compact).
        let note_content = serde_json::to_string(note)
            .map_err(|e| ChatError::InvalidResponse(format!("sérialisation note échouée: {e}")))?;

        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": note_content}
            ],
            "max_tokens": self.max_tokens,
            "temperature": 0.0
        });

        let response = self.client.post(&self.endpoint).json(&body).send().await?;

        // Propagation des erreurs HTTP non-2xx
        let response = response.error_for_status()?;

        let openai_resp: OpenAiResponse = response.json().await?;

        let content = openai_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ChatError::InvalidResponse("réponse OpenAI vide (0 choices)".into()))?
            .message
            .content;

        let verdict = extract_verdict(&content)?;

        // Validation confiance
        if !(0.0..=1.0).contains(&verdict.confidence) {
            return Err(ChatError::InvalidResponse(format!(
                "confiance hors bornes: {} (attendu 0.0-1.0)",
                verdict.confidence
            )));
        }

        let proposed_status = parse_status(&verdict.status)?;

        Ok(CuratorVerdict {
            proposed_status,
            confidence: verdict.confidence,
            reason: verdict.reason,
            backend: ChatBackend::Http,
        })
    }

    fn backend_kind(&self) -> ChatBackend {
        ChatBackend::Http
    }
}
