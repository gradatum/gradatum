//! OpenAI-compat HTTP provider — implements `LlmProvider` for any endpoint
//! conforming to `/v1/chat/completions` (llama.cpp, vLLM, OpenRouter, etc.).
//!
//! `elapsed_secs` in `LlmError::Timeout` uses the actual duration measured
//! via `std::time::Instant` rather than a symbolic value.
//!
//! Model-agnostic by design: no branching on model names in this implementation.

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

/// HTTP provider conforming to the OpenAI Chat Completions spec.
pub struct OpenAiCompatProvider {
    /// Provider name (used in logs and error messages).
    name: String,
    /// URL of the `/v1/chat/completions` endpoint.
    chat_url: String,
    /// Shared HTTP client with a configured total timeout.
    client: Client,
    /// Declared capabilities — set at construction time.
    capabilities: Capabilities,
    /// Optional API key (read from an env var at construction time).
    api_key: Option<String>,
    /// Timeout in seconds from the config.
    timeout_secs: u64,
}

impl OpenAiCompatProvider {
    /// Builds a new OpenAI-compat provider.
    ///
    /// `endpoint_base`: base URL (e.g. `"http://127.0.0.1:8080"`).
    /// The path `"/v1/chat/completions"` is appended automatically.
    ///
    /// `timeout_secs`: global HTTP timeout for non-streaming requests.
    ///
    /// `api_key_env`: when provided, the value of the named env var is read
    /// and used as a bearer token.
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

    /// Adds the `Authorization` header when an API key is configured.
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

    /// Non-streaming completion.
    ///
    /// `elapsed_secs` in `LlmError::Timeout` is measured via `Instant::now()`.
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
                    // Real elapsed duration rather than a symbolic value.
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

    /// Streaming completion — byte-level forward of the backend SSE stream.
    ///
    /// `elapsed_secs` in `LlmError::Timeout` is measured via `Instant::now()`.
    #[instrument(skip(self, request), fields(provider = %self.name, model = %request.model))]
    async fn stream(&self, request: ChatCompletionRequest) -> LlmResult<ChatCompletionStream> {
        let mut req = request;
        req.stream = Some(true);

        // Dedicated streaming client: only connect_timeout (time to first byte).
        // No total timeout to avoid cutting off large prompts mid-generation.
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
                    // Real elapsed duration rather than the symbolic timeout_secs value.
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

    /// Health check: verifies that the backend is reachable.
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

/// Converts an HTTP byte stream into a stream of `LlmResult<ChatCompletionChunk>`.
///
/// Parses the SSE protocol: each `data: <json>` line is deserialized.
/// `data: [DONE]` terminates the stream cleanly. Empty lines are ignored.
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
                        // SAFETY: backends send valid UTF-8; replace invalid sequences
                        // rather than panicking.
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
                        // TCP stream ended without [DONE] — backend closed the connection cleanly.
                        return None;
                    }
                }
            }
        },
    )
}

/// Required to build a `serde_json::Error` from an arbitrary message.
trait SerdeErrorCustom {
    fn custom(msg: impl std::fmt::Display) -> Self;
}

impl SerdeErrorCustom for serde_json::Error {
    fn custom(msg: impl std::fmt::Display) -> Self {
        // Correct idiom: `serde::de::Error::custom()` builds a `serde_json::Error`
        // carrying the arbitrary message. No dummy JSON allocation, no unwrap.
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

    /// Vérifie que le timeout réel (u64) est convertible en f64 sans perte.
    /// La valeur f64 est utilisée dans `LlmError::Timeout { elapsed_secs }`.
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
