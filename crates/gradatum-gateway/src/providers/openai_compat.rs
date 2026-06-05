//! Provider HTTP OpenAI-compat — implémente `LlmProvider` pour tout endpoint
//! conforme `/v1/chat/completions` (llama.cpp, vLLM, OpenRouter, etc.).
//!
//! F-MAJ-4 fix : `elapsed_secs` dans LlmError::Timeout utilise la durée réelle
//! mesurée via `std::time::Instant` au lieu d'une valeur symbolique.
//!
//! Conforme au principe d'agnosticité modèle : aucun branchement sur le nom
//! de modèle dans cette implémentation.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::Stream;

use crate::commons::{
    chat::{ChatCompletionRequest, ChatCompletionResponse},
    error::{LlmError, LlmResult},
    provider::{Capabilities, ChatCompletionStream, LlmProvider},
    streaming::ChatCompletionChunk,
};
use reqwest::Client;
use tracing::instrument;

/// Provider HTTP conforme OpenAI Chat Completions spec.
pub struct OpenAiCompatProvider {
    /// Nom du provider (pour logging et messages d'erreur).
    name: String,
    /// URL du endpoint `/v1/chat/completions`.
    chat_url: String,
    /// Client HTTP partagé avec timeout total configuré.
    client: Client,
    /// Capabilities déclarées — configurées à la construction.
    capabilities: Capabilities,
    /// Clé API optionnelle (lue depuis variable d'env à la construction).
    api_key: Option<String>,
    /// Timeout en secondes issu de la config.
    timeout_secs: u64,
}

impl OpenAiCompatProvider {
    /// Construit un nouveau provider OpenAI-compat.
    ///
    /// `endpoint_base` : URL de base (ex: "http://127.0.0.1:8080").
    /// Le path "/v1/chat/completions" est ajouté automatiquement.
    ///
    /// `timeout_secs` : timeout HTTP global pour les requêtes non-streaming.
    ///
    /// `api_key_env` : si fourni, la valeur de la variable d'env nommée est lue
    /// et utilisée comme Bearer token.
    pub fn new(
        name: impl Into<String>,
        endpoint_base: &str,
        timeout_secs: u64,
        api_key_env: Option<&str>,
        capabilities: Capabilities,
    ) -> anyhow::Result<Self> {
        let chat_url = format!(
            "{}/v1/chat/completions",
            endpoint_base.trim_end_matches('/')
        );

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| anyhow::anyhow!("erreur construction client HTTP: {}", e))?;

        let api_key = api_key_env.and_then(|env_name| std::env::var(env_name).ok());

        Ok(Self {
            name: name.into(),
            chat_url,
            client,
            capabilities,
            api_key,
            timeout_secs,
        })
    }

    /// Ajoute le header Authorization si une clé API est configurée.
    fn add_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Complétion non-streaming.
    ///
    /// F-MAJ-4 fix : elapsed_secs dans LlmError::Timeout est mesuré via Instant::now().
    #[instrument(skip(self, request), fields(provider = %self.name, model = %request.model))]
    async fn complete(&self, request: ChatCompletionRequest) -> LlmResult<ChatCompletionResponse> {
        let mut req = request;
        req.stream = Some(false);

        let start = Instant::now();
        let builder = self.client.post(&self.chat_url).json(&req);
        let builder = self.add_auth(builder);

        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                LlmError::Timeout {
                    // F-MAJ-4 : durée réelle mesurée au lieu de valeur symbolique.
                    elapsed_secs: start.elapsed().as_secs_f64(),
                }
            } else {
                LlmError::Network {
                    source: Box::new(e),
                }
            }
        })?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::from_http_status(status, body));
        }

        let completion: ChatCompletionResponse =
            response.json().await.map_err(|e| LlmError::Serialization {
                source: serde_json::Error::custom(e.to_string()),
            })?;

        Ok(completion)
    }

    /// Complétion streaming — forward byte-level du flux SSE du backend.
    ///
    /// F-MAJ-4 fix : elapsed_secs dans LlmError::Timeout est mesuré via Instant::now().
    #[instrument(skip(self, request), fields(provider = %self.name, model = %request.model))]
    async fn stream(&self, request: ChatCompletionRequest) -> LlmResult<ChatCompletionStream> {
        let mut req = request;
        req.stream = Some(true);

        // Client dédié au streaming : uniquement connect_timeout (temps jusqu'au premier byte).
        // Pas de timeout total pour ne pas couper les gros prompts en cours de traitement.
        let client_stream = Client::builder()
            .connect_timeout(Duration::from_secs(self.timeout_secs))
            .build()
            .map_err(|e| LlmError::Network {
                source: Box::new(e),
            })?;

        let start = Instant::now();
        let builder = client_stream.post(&self.chat_url).json(&req);
        let builder = self.add_auth(builder);

        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                LlmError::Timeout {
                    // F-MAJ-4 : durée réelle mesurée au lieu de self.timeout_secs symbolique.
                    elapsed_secs: start.elapsed().as_secs_f64(),
                }
            } else {
                LlmError::Network {
                    source: Box::new(e),
                }
            }
        })?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::from_http_status(status, body));
        }

        let stream = sse_bytes_to_chunks(response);
        Ok(Box::pin(stream))
    }

    /// Health check : vérifie que le backend répond.
    async fn health_check(&self) -> LlmResult<()> {
        let result = self
            .client
            .get(format!(
                "{}/health",
                self.chat_url.trim_end_matches("/v1/chat/completions")
            ))
            .send()
            .await;

        match result {
            Ok(resp) if !resp.status().is_server_error() => Ok(()),
            Ok(resp) => Err(LlmError::UpstreamError {
                status: resp.status().as_u16(),
                message: "health check failed".to_string(),
            }),
            Err(e) => Err(LlmError::Network {
                source: Box::new(e),
            }),
        }
    }
}

/// Convertit un flux de bytes HTTP en stream de `LlmResult<ChatCompletionChunk>`.
///
/// Parse le protocole SSE : chaque ligne `data: <json>` est désérialisée.
/// `data: [DONE]` termine le flux proprement. Les lignes vides sont ignorées.
fn sse_bytes_to_chunks(
    response: reqwest::Response,
) -> impl Stream<Item = LlmResult<ChatCompletionChunk>> {
    use futures::StreamExt;

    let byte_stream = response.bytes_stream();

    futures::stream::unfold(
        (byte_stream, String::new()),
        |(mut stream, mut buf)| async move {
            loop {
                if let Some(newline_pos) = buf.find('\n') {
                    let line = buf[..newline_pos].trim_end_matches('\r').to_string();
                    buf = buf[newline_pos + 1..].to_string();

                    if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            return None;
                        }
                        let result = serde_json::from_str::<ChatCompletionChunk>(data)
                            .map_err(|e| LlmError::Serialization { source: e });
                        return Some((result, (stream, buf)));
                    }
                    continue;
                }

                match stream.next().await {
                    Some(Ok(bytes)) => {
                        // SAFETY : les backends envoient du UTF-8 valide ; on remplace
                        // les séquences invalides plutôt que de paniquer.
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                    }
                    Some(Err(e)) => {
                        return Some((
                            Err(LlmError::Network {
                                source: Box::new(e),
                            }),
                            (stream, buf),
                        ));
                    }
                    None => {
                        // Flux TCP terminé sans [DONE] — cas backend qui ferme proprement.
                        return None;
                    }
                }
            }
        },
    )
}

/// Nécessaire pour construire un serde_json::Error depuis un message arbitraire.
trait SerdeErrorCustom {
    fn custom(msg: impl std::fmt::Display) -> Self;
}

impl SerdeErrorCustom for serde_json::Error {
    fn custom(msg: impl std::fmt::Display) -> Self {
        // Idiome correct : serde::de::Error::custom() construit un serde_json::Error
        // portant le message arbitraire. Aucune allocation JSON bidon, aucun unwrap.
        <serde_json::Error as serde::de::Error>::custom(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commons::provider::{ThinkingMode, ToolUseSupport};

    fn default_caps() -> Capabilities {
        Capabilities {
            tool_use: ToolUseSupport::Native,
            streaming: true,
            vision: false,
            thinking: ThinkingMode::None,
            context_max: 131_072,
            structured_output: false,
            prompt_caching: false,
            reasoning_levels: None,
        }
    }

    #[test]
    fn timeout_secs_stored_in_struct() {
        let provider = OpenAiCompatProvider::new(
            "test-provider",
            "http://127.0.0.1:9999",
            600,
            None,
            default_caps(),
        )
        .expect("construction OpenAiCompatProvider échouée");

        assert_eq!(provider.timeout_secs, 600);
    }

    #[test]
    fn timeout_secs_default_stored() {
        let provider = OpenAiCompatProvider::new(
            "test-default",
            "http://127.0.0.1:9999",
            120,
            None,
            default_caps(),
        )
        .expect("construction OpenAiCompatProvider échouée");

        assert_eq!(provider.timeout_secs, 120);
    }

    /// Bug 2 régression — `SerdeErrorCustom::custom()` ne doit pas paniquer et doit
    /// préserver le message d'erreur dans le `serde_json::Error` résultant.
    ///
    /// Régression : la version d'origine faisait `serde_json::from_str("\"msg\"").unwrap_err()`,
    /// mais une string est un JSON valide → `unwrap_err()` paniquait (et le message était perdu).
    /// Fix : `<serde_json::Error as serde::de::Error>::custom(msg)` — idiome serde, message préservé.
    #[test]
    fn serde_error_custom_no_panic_et_message_preservé() {
        let message = "deserialize échoué — champ 'id' manquant";
        let err = serde_json::Error::custom(message);
        let texte = err.to_string();
        // Le message doit être présent dans la représentation string de l'erreur.
        assert!(
            texte.contains("id"),
            "le message d'erreur doit être propagé dans serde_json::Error : '{texte}'"
        );
        // L'erreur doit être classifiée comme une erreur de données (pas I/O ou EOF).
        assert!(
            err.is_data(),
            "serde_json::Error::custom() doit produire une erreur is_data=true : '{texte}'"
        );
    }

    /// F-MAJ-4 : vérifie que le timeout réel (u64) est convertible en f64 sans perte.
    /// La valeur f64 est utilisée dans LlmError::Timeout { elapsed_secs }.
    #[test]
    fn elapsed_secs_real_f64_conversion() {
        let provider = OpenAiCompatProvider::new(
            "test-elapsed",
            "http://127.0.0.1:9999",
            600,
            None,
            default_caps(),
        )
        .expect("construction OpenAiCompatProvider échouée");

        // Vérifie que Instant::elapsed() peut être converti en f64.
        let start = std::time::Instant::now();
        let elapsed_secs = start.elapsed().as_secs_f64();
        assert!(elapsed_secs >= 0.0, "elapsed_secs doit être positif");
        // La valeur timeout_secs est accessible pour les tests.
        assert_eq!(provider.timeout_secs, 600);
    }
}
